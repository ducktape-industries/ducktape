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

pub(crate) mod background;
mod kernel;
mod filesystem;
mod taste;

pub use kernel::{block_hit as view_block_hit, live_hit as view_live_hit};
pub(crate) use kernel::chord_of;

/// Shared HTTP connections need a continuously driven I/O runtime. Loader
/// threads can compile or join child loads between requests; their own parked
/// runtimes would strand the pooled sockets another loader reuses.
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
use view_wire as wire;
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

/// Instruction budget for one call into a view.
const FUEL_PER_TICK: u64 = 100_000_000;
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
pub fn governance_view(dark: bool, connected: bool) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "tasting": taste_props(),
    });
    module_view(
        "governance",
        serde_json::to_vec(&props).expect("props encode"),
    )
}

/// The bell overlay's body, drawn by the `inbox` view over the KERNEL
/// CONTRACT. THE APP FOLDS NO INBOX: which notifications exist, which of
/// them are noise, how each is worded and where its `duck://` door leads
/// are the view's own reads off the inbox queue, the identity directory and
/// each source module's read lane. "Mark all read" leaves as `op.submit`,
/// signed here with the seated key. Session facts are all that go in, and
/// the two generic doors are all that come back.
///
/// The number the app paints beside its bell is the same view's count,
/// answered headless through [`background::request`] — never a second fold.
pub fn inbox_view(dark: bool, connected: bool, chain: &str, account: &str) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "chain": chain,
        "account": account,
    });
    module_view("inbox", serde_json::to_vec(&props).expect("props encode"))
}

/// The command palette, drawn by the `palette` view over the KERNEL
/// CONTRACT. THE APP HAS NO PALETTE: which chord opens it, what a query
/// searches, how a hit reads and where pressing one goes are the view's own
/// — the chord is claimed through `host.chord`, the messages and pages are
/// its own reads, and a hit leaves through `host.open_link`. The app owns
/// the seat and nothing in it: a palette that is closed draws an empty tree,
/// and an empty tree is no overlay at all.
pub fn palette_view(dark: bool, connected: bool, chain: &str) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "chain": chain,
    });
    module_view("palette", serde_json::to_vec(&props).expect("props encode"))
}

// ---------- the roster seats ----------

/// The Members tab, on the KERNEL CONTRACT: session facts go in, the view
/// reads the roster itself off the node and signs its writes through
/// `op.submit`. The one intent left is `copy` (`text`, `label`) — the
/// clipboard is an OS door the kernel has not opened.
pub fn members_view(dark: bool, connected: bool) -> ViewSpec {
    let props = serde_json::json!({
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
/// working — the rail's pulse). A chip's `duck://` address leaves through
/// the kernel's one door, not through this seat.
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
    status: &str,
    data_dir: &str,
    wall_now: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
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
    desktop_notifications_host: &str,
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
    updates: &crate::backend::update::UpdateFacts,
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
        "desktop_notifications_host": desktop_notifications_host,
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
        "tasting": taste_props(),
        "update_state": updates.state,
        "update_current": updates.current,
        "update_previous": updates.previous,
        "update_staged_display": updates.staged_display,
        "update_channel": updates.channel,
        "update_checked": updates.checked,
        "update_note": updates.note,
        "update_busy": updates.busy,
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
        "system" => Intent::System,
        "light" => Intent::Light,
        "dark" => Intent::Dark,
        "notifications" => Intent::Notifications,
        "taste" => Intent::Taste,
        "untaste" => Intent::Untaste,
        "update_check" => Intent::UpdateCheck,
        "update_restart" => Intent::UpdateRestart,
        "update_rollback" => Intent::UpdateRollback,
        _ => Intent::Copy,
    }
}

/// The rail tab a `tab` intent names; the two the settings cards link to.
pub fn settings_event_tab(event: &ModuleViewEvent) -> crate::ShellTab {
    match event_text(event, "tab").as_str() {
        "members" => crate::ShellTab::View("members"),
        _ => crate::ShellTab::View("node"),
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
        "network_chain_id": network_chain_id,
        "connected_rpc": connected_rpc,
        "link": link,
        "link_tick": link_tick,
    });
    module_view("forge", serde_json::to_vec(&props).expect("props encode"))
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
    active_channel: &'a str,
    dm_peer: &'a str,
    dm_serial: i64,
    land_seq: i64,
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
    /// Peer observations passed unchanged to the Chat view.
    call_peers: &'a [crate::call::CallPeer],
    shift_held: bool,
    copy_chord_serial: i64,
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
/// The guest owns its editor documents and product actions.
// Each parameter is a distinct piece of chat state the caller already tracks
// separately; bundling them into a struct is a structural change, not this
// clippy sweep's job.
#[allow(clippy::too_many_arguments)]
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
    active_channel: &str,
    dm_peer: &str,
    dm_serial: i64,
    land_seq: i64,
    mutation_phase: crate::MutationPhase,
    loading: bool,
    huddle_joined: bool,
    huddle_channel: &str,
    huddle_channel_name: &str,
    huddle_joined_at: i64,
    huddle_now: i64,
    call_muted: bool,
    call_speaking: bool,
    call_peers: &[crate::call::CallPeer],
    shift_held: bool,
    copy_chord_serial: i64,
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
        active_channel,
        dm_peer,
        dm_serial,
        land_seq,
        busy: mutation_phase != crate::MutationPhase::Idle,
        loading,
        huddle_joined,
        huddle_channel,
        huddle_channel_name,
        huddle_joined_at,
        huddle_now,
        call_muted,
        call_speaking,
        call_peers,
        shift_held,
        copy_chord_serial,
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

/// The act a chat intent names. The door ([`intents_of`]) refuses every kind
/// this does not list, so the wildcard is unreachable in practice; it verdicts
/// the link handler, which refuses an empty address.
pub fn chat_intent(event: &ModuleViewEvent) -> crate::ChatIntent {
    use crate::ChatIntent as Intent;
    match event.kind.as_str() {
        "show_huddle" => Intent::ShowHuddle,
        "leave_huddle" => Intent::LeaveHuddle,
        "join_huddle" => Intent::JoinHuddle,
        "join_voice" => Intent::JoinVoice,
        _ => Intent::Copy,
    }
}

// ---------- the files seat ----------

/// The Files tab, drawn by the `files` view over the KERNEL CONTRACT: the app
/// pushes session facts only — `account` among them, the reader's own, which
/// names her home under `/home` for the view's sidebar — and the view lists
/// the directory, reads the preview, walks the snapshot history and diffs a
/// snapshot for itself through `files.get` / `rpc.live`, writing through
/// `op.submit` signed here with the seated key. Two events come back, both OS doors the app owns: `open_link`
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
    account: &str,
    route: &str,
    route_serial: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "chain": chain,
        "account": account,
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

fn surface_allowed(surface: &str) -> bool {
    matches!(surface, "artifact_svg" | "artifact_image" | "picture" | "code" | "markdown")
}

/// The operations a view may ask of the app, by module. An intent outside
/// the list is refused at the door, never handed to a handler.
pub(crate) fn intents_of(module: &str) -> &'static [&'static str] {
    match module {
        // the governance view speaks the kernel contract: its writes are
        // `op.submit`. What is left at the door is a member's own taste of
        // a proposed view — a device preference, never a write
        "governance" => &["taste", "untaste"],
        // members speaks it too; `copy` is the clipboard door, not a write
        "members" => &["copy"],
        // the agents view speaks the kernel contract end to end: pause,
        // save and registration are `op.submit`, the rail's pulse is
        // `host.badge` and a chip's address is `host.open_link`. Nothing is
        // left at this seat's door.
        "agents" => &[],
        // the node view speaks the kernel contract: it reads the node's own
        // status, peers, registry and log ring itself and retunes the live
        // tracing filter through `rpc.admin`. `copy` is the clipboard door.
        "node" => &["copy"],
        // the explorer view reads and searches through the kernel: the only
        // thing it asks the app for is the clipboard
        "explorer" => &["copy"],
        // the chat view reads its own room and signs its own writes; what is
        // left at the door is what another plane of the app steers or owns
        // `share` carries the row index of the share target the picker was
        // pressed on: only the host can enumerate a display or a window.
        "call" => &[
            "mute",
            "camera",
            "screen",
            "share",
            "share_cancel",
            "channel",
            "leave",
        ],
        "chat" => &[
            "show_huddle",
            "leave_huddle",
            "join_huddle",
            "join_voice",
            "copy",
        ],
        // the forge view reads, folds and writes through the kernel: what is
        // left at the door is the two OS doors.
        "forge" => &["copy"],
        // the files view speaks the kernel contract: its reads and writes go
        // through the kernel, and the two events left are the app's own doors
        // the files view speaks the kernel contract too; a link the Markdown
        // reader activated leaves through the kernel's door.
        "files" => &[],
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
            "system",
            "light",
            "dark",
            "notifications",
            "taste",
            "untaste",
            "update_check",
            "update_restart",
            "update_rollback",
        ],
        // pages speaks the kernel contract: every read is `rpc.view` and
        // every write `op.submit`. What is left are the two OS doors — the
        // clipboard, and the open plane a `duck://` link in a document goes
        // through.
        "pages" => &["copy"],
        // a registry-listed view (no arm above) speaks the kernel contract
        // and the two OS doors every view is allowed: nothing of its own
        _ => GENERIC_DOORS,
    }
}

/// The one OS door the app owns for every registry-listed view: the
/// clipboard. Opening a `duck://` address is not here — it is the kernel's
/// `host.open_link`, which every view reaches whether or not the app has
/// heard of it ([`host_door`]).
const GENERIC_DOORS: &[&str] = &["copy"];

/// The address a kernel door asked the app to open, or `None` for an intent
/// the view itself owns.
///
/// ONE DOOR OUT, FOR EVERY VIEW. A view cannot name a tab or a route: it
/// hands over a `duck://` address and the app resolves it through the single
/// module table. The check sits here, in front of every per-view route, so a
/// view's own intent enum never grows an opening act again — and
/// `no_view_declares_its_own_open_link` fails the build if one does.
pub fn host_door(event: &ModuleViewEvent) -> Option<String> {
    if event.kind != "open_link" {
        return None;
    }
    let link = event_text(event, "link");
    (!link.is_empty()).then_some(link)
}

// ---------- the registry-listed seats ----------

/// A view the registry lists as a `Kind::View` entry: the app knows nothing
/// of it but its id, so its props are the session basics every view gets
/// (the same four the files view is sent), and its events are the two
/// generic doors. The tab it draws is named by its manifest and iconed by
/// its `icons/tab.svg` asset.
pub fn registered_view(
    module: &'static str,
    dark: bool,
    connected: bool,
    chain: &str,
    account: &str,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "chain": chain,
        "account": account,
    });
    module_view(module, serde_json::to_vec(&props).expect("props encode"))
}

/// Which module holds each claimed chord, and how many live subscriptions
/// hold it. Global, because the claim is: two seats answering to one chord
/// would make the key mean whichever guest the shell reached first that
/// frame.
///
/// The COUNT is what lets a chord move. A claim is made per subscription and
/// given back per subscription — including the ones a torn-down guest still
/// had — so a swap, in which the replacement claims before the instance it
/// replaces is dropped, leaves the holder standing rather than releasing the
/// key out from under the view that just took it.
type ChordClaims = std::collections::BTreeMap<String, (&'static str, usize)>;

fn chord_claims() -> &'static Mutex<ChordClaims> {
    static CLAIMS: OnceLock<Mutex<ChordClaims>> = OnceLock::new();
    CLAIMS.get_or_init(Mutex::default)
}

/// Claim `chord` for `module`, or name the module that already holds it.
///
/// FIRST COME HOLDS IT, and a module re-claiming its own is another claim on
/// the same key: a swap installs a new instance of the same id, and a view
/// must not lose its chord by being replaced with itself.
pub(crate) fn claim_chord(chord: &str, module: &'static str) -> Result<(), &'static str> {
    let mut claims = chord_claims().lock().expect("chord claims");
    match claims.get_mut(chord) {
        Some((holder, _)) if *holder != module => Err(holder),
        Some((_, held)) => {
            *held += 1;
            Ok(())
        }
        None => {
            claims.insert(chord.to_owned(), (module, 1));
            Ok(())
        }
    }
}

/// Give one claim on `chord` back. The key is free again once the last one
/// is given back — a chord nothing is listening for is a chord the next view
/// may have, and a claim that outlived its guest would make the key
/// unclaimable until the app restarted.
pub(crate) fn release_chord(chord: &str, module: &'static str) {
    let mut claims = chord_claims().lock().expect("chord claims");
    let Some((holder, held)) = claims.get_mut(chord) else {
        return;
    };
    if *holder != module {
        return;
    }
    *held -= 1;
    if *held == 0 {
        claims.remove(chord);
    }
}

/// The module that holds `chord`, if any — what the shell asks before it
/// carries a press to a seat.
pub(crate) fn chord_holder(chord: &str) -> Option<&'static str> {
    chord_claims()
        .lock()
        .expect("chord claims")
        .get(chord)
        .map(|(holder, _)| *holder)
}

/// The ids the connected node's registry lists as `Kind::View` entries, in
/// the registry's (id) order: the tabs the shell draws after the built-in
/// ones. Set by [`connected`] and moved by [`deployments_checked`]; empty
/// off any node.
fn registered() -> &'static Mutex<Vec<&'static str>> {
    static REGISTERED: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    REGISTERED.get_or_init(Mutex::default)
}

pub fn registered_views() -> Vec<&'static str> {
    registered().lock().expect("registered views").clone()
}

/// Every id the registry lists, view or module, in registry order: the
/// planes the live lane subscribes to beyond the built-in ones (a registered
/// view reads a registered module's plane through `rpc.live`). Set beside
/// [`registered`], from the same registry read; empty off any node.
fn registered_modules() -> &'static Mutex<Vec<String>> {
    static MODULES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    MODULES.get_or_init(Mutex::default)
}

pub fn registered_module_ids() -> Vec<String> {
    registered_modules().lock().expect("registered modules").clone()
}

/// The name a registered view's tab shows: its manifest's, once the view is
/// seated; its id until then.
pub fn registered_view_name(module: &'static str) -> String {
    let mounted = mounted(module);
    let locked = mounted.lock().expect("module view lock");
    match &locked.slot {
        Slot::Ready(guest) if !guest.name.is_empty() => guest.name.clone(),
        _ => module.to_owned(),
    }
}

/// The `icons/tab.svg` a registered view ships beside itself, once seated.
pub fn registered_view_icon(module: &'static str) -> Option<Arc<Vec<u8>>> {
    let mounted = mounted(module);
    let locked = mounted.lock().expect("module view lock");
    let Slot::Ready(guest) = &locked.slot else {
        return None;
    };
    guest
        .assets
        .get("icons/tab.svg")
        .map(|bytes| Arc::new(bytes.clone()))
}

// ---------- tasting ----------

/// The proposed hash `module`'s seat tastes, if it tastes one.
pub fn tasting(module: &'static str) -> Option<[u8; 32]> {
    let mounted = mounted(module);
    let locked = mounted.lock().expect("module view lock");
    locked.tasting
}

/// The tab's name with its taste on it: `Chat · proposed` while the seat
/// tastes a proposed view.
pub fn tab_label(module: &'static str, name: &str) -> String {
    match tasting(module) {
        Some(_) => format!("{name} · proposed"),
        None => name.to_owned(),
    }
}

/// This device tastes `hash` for `module` from now on: the seat loads the
/// proposed frame's view in place of the active one, and the preference
/// is kept for the chain. A pair the taste set refuses, or does not name,
/// is not seated and says why in the log. `module` is a view's word: it
/// names a seat, or nothing happens.
pub fn taste(module: &str, hash: [u8; 32]) -> Loads {
    let Some(module) = seated_id(module) else {
        log_source(module, Some(&hash), "Refused", 0, "not_seated");
        return Loads(Vec::new());
    };
    if let Some(reason) = taste::refused(module, hash) {
        log_source(module, Some(&hash), "Refused", 0, reason);
        return Loads(Vec::new());
    }
    taste::remember(&taste::chain_id(), module, hash);
    Loads(retaste(module, Some(hash)))
}

/// This device tastes nothing for `module` from now on: the seat returns
/// to the active view.
pub fn untaste(module: &str) -> Loads {
    let Some(module) = seated_id(module) else {
        log_source(module, None, "Refused", 0, "not_seated");
        return Loads(Vec::new());
    };
    taste::forget(&taste::chain_id(), module);
    Loads(retaste(module, None))
}

/// The seat registry's own key for `module`, if it has a seat: a view's
/// word for a module is never interned on its say-so.
fn seated_id(module: &str) -> Option<&'static str> {
    registry()
        .lock()
        .expect("module views")
        .keys()
        .copied()
        .find(|seated| *seated == module)
}

/// The seat's taste set to `wanted`, and a load after it started under a
/// new generation — so a load after the previous taste dies at install.
fn retaste(module: &'static str, wanted: Option<[u8; 32]>) -> Vec<std::thread::JoinHandle<()>> {
    // lock order, as everywhere: the registry, then the seat; the
    // connection is read and let go between them
    let registry = registry().lock().expect("module views");
    let snapshot = connection().lock().expect("views rpc").clone();
    let Some(mounted) = registry.get(module).cloned() else {
        return Vec::new();
    };
    let mut locked = mounted.lock().expect("module view lock");
    let unchanged = locked.tasting == wanted;
    if unchanged {
        return Vec::new();
    }
    locked.tasting = wanted;
    let generation = locked.start(wanted);
    drop(locked);
    vec![spawn_load(module, &mounted, generation, snapshot)]
}

/// The taste set as the governance and settings views draw it: one row
/// per tasteable pair, with where it stands, whether this seat tastes it,
/// and why it cannot be tasted if it cannot.
fn taste_props() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = taste::rows()
        .into_iter()
        .map(|row| {
            let (status, activation_height) = match row.stage {
                crate::backend::view_source::Stage::Open => ("open", 0),
                crate::backend::view_source::Stage::Scheduled { activation_height } => {
                    ("scheduled", activation_height)
                }
            };
            let tasting = tasting(row.module) == Some(row.hash);
            serde_json::json!({
                "module": row.module,
                "name": module_name(row.module),
                "proposal": row.proposal.unwrap_or_default(),
                "hash": crate::backend::hex_encode(&row.hash),
                "status": status,
                "activation_height": activation_height,
                "tasting": tasting,
                "reason": row.refusal.map_or("", taste::Refusal::reason),
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// The name a module's tab shows: the built-in tabs' own, a registered
/// view's manifest name.
pub fn module_name(module: &'static str) -> String {
    match module {
        "chat" => "Chat".to_owned(),
        "pages" => "Pages".to_owned(),
        "forge" => "Forge".to_owned(),
        "agents" => "Agents".to_owned(),
        "files" => "Files".to_owned(),
        "explorer" => "Explorer".to_owned(),
        "node" => "Node".to_owned(),
        "members" => "Members".to_owned(),
        "governance" => "Governance".to_owned(),
        "settings" => "Settings".to_owned(),
        registered => registered_view_name(registered),
    }
}

/// The notices a taste leaves for the member — withdrawn, activated — as
/// the stream the app's toast subscribes to.
pub fn notices() -> impl futures::Stream<Item = String> {
    taste::notice_stream()
}

/// The seats' tastes reconciled with the taste set just walked: a seat
/// tasting a hash the set no longer names is returned to the active view
/// — silently when the hash IS the active one now (the seat already
/// draws it), with a notice either way. Under the registry lock.
fn reconcile_tastes(
    registry: &HashMap<&'static str, Arc<Mutex<Mounted>>>,
    hashes: &std::collections::BTreeMap<String, crate::backend::view_source::Entry>,
) {
    for (module, mounted) in registry {
        let mut locked = mounted.lock().expect("module view lock");
        let Some(tasted) = locked.tasting else {
            continue;
        };
        if taste::listed(module, tasted) {
            continue;
        }
        let active = hashes.get(*module).and_then(|entry| entry.hash);
        let activated = active == Some(tasted);
        locked.tasting = None;
        taste::forget(&taste::chain_id(), module);
        let name = module_name(module);
        let (reason, sentence) = match activated {
            true => (
                "taste_activated",
                format!("The {name} view you were trying is now the current view"),
            ),
            false => (
                "taste_withdrawn",
                format!("The proposed {name} view was withdrawn — back to the current view"),
            ),
        };
        log_source(module, Some(&tasted), "Untasted", locked.generation, reason);
        taste::notice(sentence);
    }
}

/// The device's remembered tastes for this chain, seated: every module
/// whose remembered hash the taste set names and does not refuse starts a
/// load after it. Under the registry lock.
fn seat_remembered_tastes(
    registry: &HashMap<&'static str, Arc<Mutex<Mounted>>>,
    asked_of: &Connection,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut loads = Vec::new();
    for (module, hash) in taste::remembered(&taste::chain_id()) {
        let module = intern(&module);
        let Some(mounted) = registry.get(module) else {
            continue;
        };
        if let Some(reason) = taste::refused(module, hash) {
            log_source(module, Some(&hash), "Refused", 0, reason);
            continue;
        }
        let mut locked = mounted.lock().expect("module view lock");
        let already = locked.tasting == Some(hash);
        if already {
            continue;
        }
        locked.tasting = Some(hash);
        let generation = locked.start(Some(hash));
        drop(locked);
        loads.push(spawn_load(module, mounted, generation, asked_of.clone()));
    }
    loads
}

/// A registry id as the `&'static str` the seat registry, the tab and the
/// guest are keyed by. Leaked ONCE per distinct id: the set is the ids a
/// network's registry lists, bounded by its module set, and a repeat
/// answers the same leak — the same interning `bin/node` does for module
/// labels.
pub(crate) fn intern(id: &str) -> &'static str {
    static INTERNED: OnceLock<Mutex<std::collections::BTreeSet<&'static str>>> = OnceLock::new();
    let mut interned = INTERNED
        .get_or_init(Mutex::default)
        .lock()
        .expect("interned ids");
    if let Some(known) = interned.get(id) {
        return known;
    }
    let leaked: &'static str = Box::leak(id.to_owned().into_boxed_str());
    interned.insert(leaked);
    leaked
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
    // before any view is touched — the connection lock is a leaf and no
    // view lock is ever taken under it. Lock order, everywhere: registry,
    // then a view.
    let snapshot = {
        let mut connection = connection().lock().expect("views rpc");
        connection.rev += 1;
        connection.client = Some(client.clone());
        connection.clone()
    };
    for mounted in registry.values() {
        mounted
            .lock()
            .expect("module view lock")
            .changes
            .send_replace(());
    }
    for module in crate::backend::view_source::MODULE_OWNED {
        registry.entry(module).or_insert_with(Mounted::seat);
    }
    // the registry-listed views of the node just left are not this node's:
    // their seats and tabs go now, and this node's come back once its own
    // registry has been read below
    for retired in registered().lock().expect("registered views").drain(..) {
        if let Some(retired) = registry.remove(retired) {
            let mut retired = retired.lock().expect("module view lock");
            retired.slot = Slot::Empty;
            retired.changes.send_replace(());
        }
    }
    registered_modules().lock().expect("registered modules").clear();
    let mut loads: Vec<std::thread::JoinHandle<()>> = registry
        .iter()
        .filter_map(|(module, mounted)| {
            let mut locked = mounted.lock().expect("module view lock");
            let desktop_view = crate::backend::view_source::desktop_owned(module);
            let seated = matches!(locked.slot, Slot::Ready(_));
            // the desktop's own view is the same on every node: one seated,
            // or on its way from the staged file, is left alone
            let left_alone = desktop_view && (seated || locked.in_flight);
            if left_alone {
                if let Slot::Ready(guest) = &mut locked.slot {
                    guest.reconnect(snapshot.rev);
                }
                return None;
            }
            // a node-owned view is reloaded from this node's deployment;
            // the seat keeps what it shows until that lands. A taste is
            // the previous chain's: this chain's is re-read from the
            // preference once its registry has answered
            locked.tasting = None;
            let generation = locked.start(None);
            Some(spawn_load(module, mounted, generation, snapshot.clone()))
        })
        .collect();
    // the views this node's registry lists and no built-in tab draws are
    // found off the registry itself, on their own thread, and their loads
    // are joined by it — so the connect settles with them in hand too
    loads.push(spawn_registry_read(snapshot));
    Loads(loads)
}

/// Reads the node's registry for its `Kind::View` entries, seats each one
/// the registry does not hold yet, loads them off this node and joins the
/// loads. Nothing is seated if the app has moved to another node meanwhile.
fn spawn_registry_read(asked_of: Connection) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(client) = asked_of.client.as_ref() else {
            return;
        };
        let runtime = runtime();
        let entries = match runtime.block_on(crate::backend::view_source::active_hashes(client)) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::app",
                    reason = "module_status_unreadable",
                    error = %error,
                    "registry-listed views not seated"
                );
                return;
            }
        };
        // the taste set for this chain, and what this device remembers
        // tasting on it: a walk that fails seats no taste, and says so
        let taste_walk = runtime.block_on(taste::refresh(client, &entries));
        let loads = {
            let mut registry = registry().lock().expect("module views");
            let node_since_left = connection().lock().expect("views rpc").rev != asked_of.rev;
            if node_since_left {
                return;
            }
            let mut loads = seat_registered_views(&mut registry, &entries, &asked_of);
            match taste_walk {
                Ok(_chain) => loads.extend(seat_remembered_tastes(&registry, &asked_of)),
                Err(error) => tracing::warn!(
                    target: "ducktape::app",
                    reason = "taste_set_unreadable",
                    error = %error,
                    "remembered tastes not seated"
                ),
            }
            loads
        };
        for load in loads {
            load.join().expect("a registered view load");
        }
    })
}

/// The registry's `Kind::View` entries become the registered tab set, in
/// registry order; one not seated yet is seated and loaded, one no longer
/// listed is dropped with its seat (its tab disappears). The built-in and
/// desktop seats are never touched here. Under the registry lock.
fn seat_registered_views(
    registry: &mut HashMap<&'static str, Arc<Mutex<Mounted>>>,
    entries: &std::collections::BTreeMap<String, crate::backend::view_source::Entry>,
    asked_of: &Connection,
) -> Vec<std::thread::JoinHandle<()>> {
    use crate::backend::view_source::Kind;
    let listed: Vec<&'static str> = entries
        .iter()
        .filter(|(_, entry)| match entry.kind {
            Kind::View => true,
            Kind::Module => false,
        })
        .map(|(id, _)| intern(id))
        .collect();
    let mut registered = registered().lock().expect("registered views");
    for retired in registered.iter().filter(|id| !listed.contains(id)) {
        if let Some(retired) = registry.remove(retired) {
            let mut retired = retired.lock().expect("module view lock");
            retired.slot = Slot::Empty;
            retired.changes.send_replace(());
        }
    }
    let mut loads = Vec::new();
    for module in &listed {
        let seat = registry.entry(module).or_insert_with(Mounted::seat);
        let mut locked = seat.lock().expect("module view lock");
        let asked = locked.generation > 0;
        if asked {
            continue;
        }
        let generation = locked.start(None);
        drop(locked);
        loads.push(spawn_load(module, seat, generation, asked_of.clone()));
    }
    *registered = listed;
    *registered_modules().lock().expect("registered modules") = entries.keys().cloned().collect();
    loads
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
    let taste_walk = taste::refresh(client, &hashes).await.map(|_chain| ());
    let mut registry = registry().lock().expect("module views");
    // the connection may have moved while the registry answered: a load
    // asked of the old one dies at install, and `connected` restarted
    // everything under the new one
    if connection().lock().expect("views rpc").rev != asked_of.rev {
        return Loads(Vec::new());
    }
    // a block may have activated a registry-listed view (seated here) or
    // retired one (its seat and tab go); the built-in seats reload below
    let mut loads = seat_registered_views(&mut registry, &hashes, &asked_of);
    // the same block may have opened, settled or withdrawn a code
    // proposal: the taste set is walked on the same tick, and a seat whose
    // taste left it returns to the active view. A walk that fails leaves
    // every taste as it was: nothing is known to have left
    match taste_walk {
        Ok(()) => reconcile_tastes(&registry, &hashes),
        Err(error) => tracing::warn!(
            target: "ducktape::app",
            reason = "taste_set_unreadable",
            error = %error,
            "proposed views not checked"
        ),
    }
    let reloads = registry
        .iter()
        .filter(|(module, _)| !crate::backend::view_source::desktop_owned(module))
        .filter_map(|(module, mounted)| {
            let mut locked = mounted.lock().expect("module view lock");
            // a module the node does not run has no deployment to move
            // to: the view stays as `connected` left it
            let active = hashes.get(*module).copied()?.hash;
            // the seat is after its taste, else the active deployment
            let wanted = locked.tasting.or(active);
            // a load already after this deployment — or after whatever is
            // active, as a reconnect's is — lands or fails on its own;
            // starting over on every block would never let it land
            let waited_for = locked.in_flight
                && locked
                    .wanted
                    .is_none_or(|in_flight_after| Some(in_flight_after) == wanted);
            // a candidate that just failed is left alone until its gap is
            // up: the same bytes fail the same way, so one attempt a block
            // is unbounded work for a deployment that never lands
            if waited_for || wanted == locked.hash || locked.held_off(wanted) {
                return None;
            }
            let Some(replacement) = locked.replacement_after_wait() else {
                log_source(
                    module,
                    wanted.as_ref(),
                    "Failed",
                    locked.generation,
                    "replacement_waiting",
                );
                return None;
            };
            let generation = locked.start(wanted);
            locked.replacement = replacement;
            Some(spawn_load(module, mounted, generation, asked_of.clone()))
        });
    loads.extend(reloads);
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

/// THE CONNECTION LOCK IS A LEAF: read or write what you need and let it
/// go — never hold this guard while taking the registry's or a seat's. The
/// window thread draws under a seat's lock, and a guest call inside that
/// redraw asks for the client here, so anything that waits for a seat while
/// holding this closes the cycle and wedges the app. Lock order everywhere
/// else: the registry, then a seat. A snapshot read here goes stale the
/// moment it is let go, and that is fine: a load installs only under the
/// generation the seat still waits for.
fn connection() -> &'static Mutex<Connection> {
    static CONNECTION: OnceLock<Mutex<Connection>> = OnceLock::new();
    CONNECTION.get_or_init(Mutex::default)
}

struct Mounted {
    changes: tokio::sync::watch::Sender<()>,
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
    /// The proposed frame this device tastes in place of the active one:
    /// the seat's wanted hash is `tasting.or(active)`. Cleared, with a
    /// notice, when the hash leaves the taste set — withdrawn, or
    /// activated into the very hash the seat already draws.
    tasting: Option<[u8; 32]>,
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
            changes: tokio::sync::watch::channel(()).0,
            slot: Slot::Loading,
            props: None,
            generation: 0,
            hash: None,
            in_flight: false,
            wanted: None,
            tasting: None,
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
        self.changes.send_replace(());
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
/// for a module's view, the app is still on the node it was asked of. The
/// desktop's own view is the same on every node: its load lands wherever
/// the app has moved to meanwhile.
fn spawn_load(
    module: &'static str,
    mounted: &Arc<Mutex<Mounted>>,
    generation: u64,
    asked_of: Connection,
) -> std::thread::JoinHandle<()> {
    let loading = mounted.clone();
    std::thread::spawn(move || {
        let loaded = Guest::load(module, asked_of.client.as_ref(), generation, &loading);
        let mut locked = loading.lock().expect("module view lock");
        if locked.generation != generation {
            return;
        }
        locked.in_flight = false;
        // the connection lock is a leaf: read under the seat's lock, never
        // across it. A connect bumps the revision before it reaches this
        // seat, so the revision read here is the one the seat installs
        // against — and `connected` reconnects a seat it passed already
        let current_rev = connection().lock().expect("views rpc").rev;
        let from_the_node = !crate::backend::view_source::desktop_owned(module);
        let node_since_left = current_rev != asked_of.rev;
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
            Ok(Loaded::Unchanged) => {
                if let Slot::Ready(guest) = slot {
                    guest.reconnect(current_rev);
                }
            }
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
        locked.changes.send_replace(());
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
    /// Native backend integration tests use the actual staged Chat guest.
    pub(crate) fn stage_chat(client: &ducktape_rpc::Client) {
        {
            let mut connection = super::connection().lock().unwrap();
            connection.client = Some(client.clone());
            connection.rev += 1;
        }
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/views/chat_view.wasm");
        let guest = super::Guest::load_from("chat", &path).expect("build the current Chat view");
        let source = super::mounted("chat");
        let mut source = source.lock().unwrap();
        source.slot = super::Slot::Ready(Box::new(guest));
        source.generation += 1;
    }

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
            if let super::wire::Node::Surface { name, args, .. } = node
                && matches!(name.as_str(), "artifact_svg" | "artifact_image")
            {
                *node = super::surfaces::asset_node(name, args, guest);
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

/// What a fresh instance did with the state the drawn view left it. A trap
/// is not one of these — it takes the instance with it and is the load's
/// error. A refusal is the guest's own word, reported before it builds
/// anything, so the instance is untouched and can start clean instead.
enum Restored {
    Carried,
    Refused(String),
}

struct Guest {
    /// Shared immutable deployed code; isolated sessions own independent stores.
    component: Component,
    /// Node requests belong to the network selected when this instance starts.
    connection_rev: u64,
    session: Option<background::Attachment>,
    user_activation: Option<()>,
    sessions: std::collections::BTreeMap<u64, tokio::sync::watch::Sender<Vec<u8>>>,
    module: &'static str,
    /// The manifest's name: what a registered view's tab is called.
    name: String,
    store: Store<HostState>,
    tick: TypedFunc<(Vec<u8>,), (Vec<u8>,)>,
    /// The guest's events for its next tick.
    pending: Vec<wire::Event>,
    /// Requests wait for the native layout of a frame that still mounts
    /// their target, inside this instance only.
    widget_commands: Vec<(u64, wire::WidgetCommand)>,
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
    visible: bool,
    visibility_change: Option<bool>,
    visibility_subscriptions: Vec<u64>,
    /// What the guest asked the app to do this redraw.
    intents: Vec<ModuleViewEvent>,
    /// The kernel's answers to this guest's node calls, on their way in.
    replies: Arc<kernel::Replies>,
    /// The guest's `rpc.live` subscriptions, each with the plane it named:
    /// told on every block that moves that plane.
    live_subscriptions: Vec<(u64, String)>,
    /// Pending host requests and subscriptions, each owned by this guest
    /// the kernel opened for it: retired with the cancel, and with the guest.
    tasks: Vec<(u64, kernel::NodeTask)>,
    filesystem: filesystem::Filesystem,
    /// The guest's `clock.ticks` subscriptions: the period it asked for and
    /// the instant its next item is due. A module has no clock of its own,
    /// so periodic guest subscriptions use this list — driven from the window
    /// thread's own redraw, never from a thread that would have to wake it.
    clocks: Vec<kernel::Clock>,
    /// The chords this guest claimed, each with the subscription its presses
    /// arrive on. The claim itself is global ([`claim_chord`]): two seats
    /// cannot answer to one chord.
    chords: Vec<(u64, String)>,
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
        Engine::new(&config).expect("wasmtime engine")
    })
}

/// Reset the instruction allowance before entering the guest.
fn arm(store: &mut Store<HostState>) {
    let _ = store.set_fuel(FUEL_PER_TICK);
}

/// EVERY CHORD LEAVES WITH THE GUEST. The claim table is global and outlives
/// any one instance, so an instance that is retired, replaced, or ended by a
/// trap and kept its chords would make them unclaimable until the app
/// restarted — including by the view that takes its seat. This is the one
/// place that covers all three, because all three end with the box dropped.
impl Drop for Guest {
    fn drop(&mut self) {
        for (_, chord) in &self.chords {
            release_chord(chord, self.module);
        }
    }
}

impl Guest {
    /// Reusing identical code still retires work started on the old connection.
    fn reconnect(&mut self, revision: u64) {
        if self.connection_rev == revision {
            return;
        }
        let ids: Vec<_> = self.tasks.iter().map(|(id, _)| *id).collect();
        self.tasks.clear();
        self.sessions.clear();
        self.filesystem = Default::default();
        for id in ids {
            self.refuse(id, "network connection changed".into());
        }
        self.connection_rev = revision;
    }

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
        if view_source::desktop_owned(module) {
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
        let runtime = runtime();
        let started = Instant::now();
        let mut timing = LoadTiming {
            path: "first",
            ..LoadTiming::default()
        };
        let mut asked = view_source::Asked::default();
        // the seat's taste, read once: a load is after the frame the seat
        // wanted when it started, and a taste that moves meanwhile starts
        // another load under another generation
        let tasting = mounted.lock().expect("module view lock").tasting;
        let source = runtime.block_on(view_source::resolve(client, module, tasting, &mut asked));
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
            ViewSource::Missing { hash } if tasting.is_some() => {
                // a taste never empties a seat: the taste set lists such a
                // frame as `no_view` and refuses it before it gets here
                let reason = "no_view";
                logged(Some(&hash), "Failed", reason);
                timing.log(module, Some(&hash), started, "Failed");
                return Err(Unloaded {
                    hash: Some(hash),
                    reason: reason.to_owned(),
                });
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
            let component_bytes = component;
            let component = Self::compile(&component_bytes, &shown);
            timing.compile = compiled.elapsed();
            let component = component?;
            let seated = Instant::now();
            let name = manifest_name(&component_bytes);
            let prepared = (|| -> Result<Self, String> {
                let mut fresh = Self::instantiate(module, &component, &shown)?;
                fresh.name = name.clone();
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
                                match fresh.restore(&snapshot, &shown)? {
                                    Restored::Carried => {}
                                    // State this build cannot read is state
                                    // lost, not a deployment refused: the
                                    // alternative is a tab that runs the old
                                    // code until the app restarts, because
                                    // every retry offers the same snapshot.
                                    Restored::Refused(refusal) => {
                                        tracing::warn!(
                                            target: "ducktape::app",
                                            module,
                                            hash = %crate::backend::hex_encode(&hash),
                                            reason = "snapshot_refused",
                                            refusal = %refusal,
                                            "view_state_dropped"
                                        );
                                        fresh.init(&shown)?;
                                    }
                                }
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
            // the block that moved it starts another load. A tasted frame
            // is content-addressed and moves for nobody: the seat's taste
            // is the word, and a taste that moved dies at install
            let checked = Instant::now();
            let still_wanted = match tasting {
                Some(_) => Ok(true),
                None => runtime.block_on(still_active(client, module, hash)),
            };
            timing.check = checked.elapsed();
            if !still_wanted? {
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
        let tasted = match tasting {
            Some(_) => "tasting",
            None => "",
        };
        let state = match &outcome {
            Ok(Loaded::Fresh(_)) => {
                logged(Some(&hash), "Ready", tasted);
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

    fn restore(&mut self, snapshot: &[u8], shown: &str) -> Result<Restored, String> {
        arm(&mut self.store);
        let answered = self
            .restore
            .call(
                &mut self.store,
                (snapshot.to_vec(), cfg!(target_os = "macos")),
            )
            .map_err(|error| format!("{shown}: restore trapped: {}", first_line(&error)))?
            .0;
        Ok(match answered {
            Ok(()) => Restored::Carried,
            Err(refusal) => Restored::Refused(refusal),
        })
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
        guest.name = manifest_name(bytes);
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
        view_wire::manifest::read_manifest(bytes)
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
        // The `ducktape:view` world's one import is the panic hook's; anything
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
            component: component.clone(),
            connection_rev: connection().lock().expect("views rpc").rev,
            session: None,
            user_activation: None,
            sessions: Default::default(),
            module,
            name: String::new(),
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
            visible: false,
            visibility_change: None,
            visibility_subscriptions: Vec::new(),
            intents: Vec::new(),
            replies: Arc::default(),
            live_subscriptions: Vec::new(),
            tasks: Vec::new(),
            filesystem: Default::default(),
            clocks: Vec::new(),
            chords: Vec::new(),
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

    fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.visibility_change = Some(visible);
    }

    /// A chord this guest claimed was pressed: every subscription that named
    /// it gets one item. Says whether any did, which is how the shell knows
    /// the press was spent and must not also be a native key.
    fn chord_pressed(&mut self, chord: &str) -> bool {
        let claimed: Vec<u64> = self
            .chords
            .iter()
            .filter(|(_, named)| named == chord)
            .map(|(id, _)| *id)
            .collect();
        for id in &claimed {
            self.pending.push(wire::Event::Response {
                id: *id,
                result: Ok(Vec::new()),
                done: false,
            });
        }
        !claimed.is_empty()
    }

    fn sync_visibility(&mut self) {
        let Some(visible) = self.visibility_change.take() else {
            return;
        };
        for id in &self.visibility_subscriptions {
            self.pending.push(wire::Event::Response {
                id: *id,
                result: Ok(visible.to_string().into_bytes()),
                done: false,
            });
        }
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
        // Queued results precede becoming visible, so a view can distinguish
        // arrivals while hidden from data received after it returns to screen.
        self.sync_visibility();
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
        self.user_activation = None;
        for id in std::mem::take(&mut self.frame.cancels) {
            self.filesystem.cancel(id);
            self.widget_commands.retain(|(request, _)| *request != id);
            if self.props_subscription == Some(id) {
                self.props_subscription = None;
            }
            self.live_subscriptions.retain(|(live, _)| *live != id);
            self.visibility_subscriptions
                .retain(|subscription| *subscription != id);
            // dropping the stream aborts it: the node socket goes with the
            // subscription the view abandoned
            self.tasks.retain(|(task, _)| *task != id);
            self.clocks.retain(|clock| clock.id != id);
            // a chord is given back with the subscription its presses were
            // arriving on, or the next view could never claim it
            let dropped: Vec<String> = self
                .chords
                .iter()
                .filter(|(subscription, _)| *subscription == id)
                .map(|(_, chord)| chord.clone())
                .collect();
            self.chords.retain(|(subscription, _)| *subscription != id);
            for chord in dropped {
                release_chord(&chord, self.module);
            }
            self.sessions.remove(&id);
            if let Some(session) = self.session.as_mut() {
                session.media.cancel(id);
            }
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
        let finished = self
            .session
            .as_ref()
            .is_some_and(|session| !session.active());
        if finished {
            self.refuse(id, "session finished".into());
            return;
        }
        let payload_limit = match kind.as_str() {
            "op.submit_bytes" => ::node::MAX_PAYLOAD_BYTES.div_ceil(3) * 4 + 256,
            _ => MAX_PAYLOAD_BYTES,
        };
        if payload.len() > payload_limit {
            self.refuse(
                id,
                format!("`{kind}` carries more than {payload_limit} bytes"),
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
            ("host", "emit") => background::emit(self, id, payload),
            ("host", "finish") => background::finish(self, id, &payload),
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

    /// The key a command acts on, or `None` for the two that act on focus
    /// order rather than a node. Exhaustive by design: adding a command
    /// requires reviewing its scope.
    fn command_target(command: &wire::WidgetCommand) -> Option<&str> {
        use wire::WidgetCommand as C;
        match command {
            C::FocusPrevious | C::FocusNext => None,
            C::EditorAction { target, .. }
            | C::Focus { target }
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
        }
    }

    /// Whether the key this command names is in the tree the guest is
    /// showing RIGHT NOW. A press is answered a frame or more after it was
    /// made, and a live view replaces its frame between the two — so the
    /// question a queued command has to pass is whether its target is still
    /// there, never whether the frame it was made against is still the
    /// current one.
    fn target_is_mounted(&self, command: &wire::WidgetCommand) -> bool {
        fn contains(node: &wire::Node, target: &str) -> bool {
            node.key() == Some(target)
                || node.children().iter().any(|child| contains(child, target))
        }
        let Some(target) = Self::command_target(command) else {
            return true;
        };
        self.frame
            .root
            .as_ref()
            .is_some_and(|root| contains(root, target))
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
            if !self.target_is_mounted(&command) {
                return Err("widget target is outside this guest tree".into());
            }
            Ok(command)
        })();
        match admitted {
            Ok(command) => self.widget_commands.push((id, command)),
            Err(error) => self.refuse(id, error),
        }
    }

    /// Called on this guest's mounted tree once native editor work drains.
    /// A command whose target left the tree in the meantime is answered with
    /// that, never performed.
    fn execute_widget_commands(
        &mut self,
        mut execute: impl FnMut(wire::WidgetCommand) -> Result<Vec<u8>, String>,
    ) {
        for (id, command) in std::mem::take(&mut self.widget_commands) {
            let result = if self.target_is_mounted(&command) {
                execute(command)
            } else {
                Err("widget target left the tree".into())
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

/// The name a component's manifest gives it; "" for one whose manifest
/// cannot be read (`compile` refuses those before a seat).
fn manifest_name(bytes: &[u8]) -> String {
    view_wire::manifest::read_manifest(bytes)
        .map(|manifest| manifest.name)
        .unwrap_or_default()
}

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

    /// Does this seat's view draw anything at all?
    ///
    /// An overlay seat is always mounted and mostly silent: a palette that is
    /// closed returns an empty tree, and an empty tree must not be stacked
    /// over the window — a layer that draws nothing still eats the presses
    /// underneath it. This is that question, asked of the tree the guest
    /// actually sent.
    pub(crate) fn draws(&self) -> bool {
        let seat = mounted(self.module);
        let mounted = seat.lock().expect("module view lock");
        let Slot::Ready(guest) = &mounted.slot else {
            return false;
        };
        guest
            .frame
            .root
            .as_ref()
            .is_some_and(|root| !root.children().is_empty())
    }

    /// A chord was pressed: if this seat's module claimed it, its guest is
    /// told and redrawn, and the press is spent. Says whether it landed, so
    /// the shell stops at the seat that took it.
    pub(crate) fn chord(&mut self, chord: &str, cx: &mut gpui_kit::Context<Self>) -> bool {
        if chord_holder(chord) != Some(self.module) {
            return false;
        }
        let seat = mounted(self.module);
        let mut mounted = seat.lock().expect("module view lock");
        let Slot::Ready(guest) = &mut mounted.slot else {
            return false;
        };
        let taken = guest.chord_pressed(chord);
        drop(mounted);
        if taken {
            cx.notify();
        }
        taken
    }

    /// A hidden tab gets one bounded update before its native presenter leaves.
    pub(crate) fn hide(&mut self) -> Vec<ModuleViewEvent> {
        let Some(alive) = &self.alive else {
            return Vec::new();
        };
        let seat = mounted(self.module);
        let mut mounted = seat.lock().expect("module view lock");
        let Mounted { slot, props, .. } = &mut *mounted;
        let Slot::Ready(guest) = slot else {
            return Vec::new();
        };
        let owns_instance =
            guest.seated_generation() == self.generation && Arc::ptr_eq(alive, &guest.alive);
        if !owns_instance || !guest.visible {
            return Vec::new();
        }
        guest.set_visible(false);
        guest.redraw(props);
        std::mem::take(&mut guest.intents)
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
        guest.set_visible(true);
        let again = guest.redraw(props);
        filesystem::mount(guest, cx);
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
                    self.subscription =
                        Some(cx.subscribe(&content, move |this, source, event, cx| {
                            let activation = source.read(cx).take_user_activation(event);
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
                            guest.user_activation = activation;
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
                // The child tree mounts during this frame. A newly opened
                // menu or input cannot receive focus before that render.
                window.defer(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let mut locked = seat.lock().expect("module view lock");
                        let Slot::Ready(guest) = &mut locked.slot else {
                            return;
                        };
                        // The GUEST has to be the one that asked — a
                        // replacement or a reseat takes its queue with it.
                        // The FRAME does not: a live view replaces its frame
                        // between the press and this deferred run, and each
                        // command is judged against the tree standing now.
                        let same_guest = guest.seated_generation() == generation
                            && Arc::ptr_eq(&alive, &guest.alive);
                        if !same_guest {
                            return;
                        }
                        if guest.inputs.pending() {
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
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::editor::wire::init_notion(cx);
        });
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

    /// A CHORD CAN MOVE, or the first view to claim one would own it for the
    /// life of the process — and a swap moving a chord from one module to
    /// another is the whole point of a chord being a view's to declare. The
    /// claim is per subscription and given back per subscription, which is
    /// also what keeps a swap safe: the replacement claims before the
    /// instance it replaces is dropped, and the key must not go free in
    /// between.
    #[test]
    fn a_chord_moves_on_once_every_claim_on_it_is_given_back() {
        // This test's own key: the table is global and keyed by the chord,
        // so a shared name would be a race with every other test.
        let chord = "cmd-alt-f7";
        assert_eq!(claim_chord(chord, "chat"), Ok(()));
        assert_eq!(claim_chord(chord, "pages"), Err("chat"));
        // a swap: the fresh instance of the same id claims while the one it
        // replaces still holds, and dropping that one leaves the key held
        assert_eq!(claim_chord(chord, "chat"), Ok(()));
        release_chord(chord, "chat");
        assert_eq!(chord_holder(chord), Some("chat"));
        // and a module that never held it cannot give it away
        release_chord(chord, "pages");
        assert_eq!(chord_holder(chord), Some("chat"));
        release_chord(chord, "chat");
        assert_eq!(chord_holder(chord), None);
        assert_eq!(claim_chord(chord, "pages"), Ok(()));
        assert_eq!(chord_holder(chord), Some("pages"));
        release_chord(chord, "pages");
    }

    /// The same, driven through the kernel and a real guest: a chord a view
    /// claimed is refused to the next one WHILE it is seated, and is the next
    /// one's the moment that guest ends. Retired, replaced, trapped — all
    /// three end with the box dropped, which is where the claim goes back.
    #[test]
    fn a_chord_is_the_next_views_once_the_guest_holding_it_ends() {
        let (Some(first), Some(second)) = (staged("members"), staged("node")) else {
            return;
        };
        let _turn = blocking_connection_turn();
        let chord = "cmd-shift-f8";
        // The refusal the kernel gave for `id`, or None if it took the claim.
        let refusal = |guest: &mut Guest, id: u64| {
            guest.replies.wait_idle();
            guest
                .replies
                .drain_into(&mut guest.pending)
                .expect("bounded replies");
            guest.pending.drain(..).find_map(|event| match event {
                wire::Event::Response { id: at, result, .. } if at == id => result.err(),
                _ => None,
            })
        };

        let mut members = Guest::load_from("members", &first).expect("the view loads");
        assert!(kernel::answer(
            &mut members,
            "host",
            "chord",
            7,
            chord.as_bytes()
        ));
        assert_eq!(refusal(&mut members, 7), None);
        assert_eq!(chord_holder(chord), Some("members"));

        // A second seat asking is refused BY NAME, so a swap that wanted a
        // taken chord says so instead of going quiet.
        let mut node = Guest::load_from("node", &second).expect("the view loads");
        assert!(kernel::answer(&mut node, "host", "chord", 7, chord.as_bytes()));
        assert_eq!(
            refusal(&mut node, 7).as_deref(),
            Some("`cmd-shift-f8` is already members's")
        );
        assert!(!node.chord_pressed(chord), "a refused claim hears nothing");

        drop(members);
        assert_eq!(chord_holder(chord), None, "the key went with the guest");
        assert!(kernel::answer(&mut node, "host", "chord", 8, chord.as_bytes()));
        assert_eq!(refusal(&mut node, 8), None);
        assert_eq!(chord_holder(chord), Some("node"));
        assert!(node.chord_pressed(chord), "the press reaches the new holder");
        drop(node);
        assert_eq!(chord_holder(chord), None);
    }

    /// Only the operations a module declares reach the app; the props
    /// subscription and the log are the host's, everything else is refused.
    /// The governance view declares none: it speaks the kernel contract.
    #[test]
    fn only_declared_intents_are_routed() {
        assert_eq!(intents_of("governance"), ["taste", "untaste"]);
        // a registry-listed view declares nothing of its own: the one
        // generic OS door, and only that
        assert_eq!(intents_of("home"), GENERIC_DOORS);
        assert_eq!(GENERIC_DOORS, ["copy"]);
        assert_eq!(intents_of("members"), ["copy"]);
        // the agents view signs its own pause and save through `op.submit`,
        // pulses the rail through `host.badge` and opens an address through
        // `host.open_link`: nothing is left at its door
        assert_eq!(intents_of("agents"), [] as [&str; 0]);
        let chat = intents_of("chat");
        assert_eq!(chat.len(), 5);
        assert!(!chat.contains(&"scrolled"), "scrolling belongs to the view");
        // the writes the view signs for itself are nobody's intent
        for signed in ["react", "edit", "delete", "rename", "search", "mark_read"] {
            assert!(!chat.contains(&signed), "{signed} is an op.submit now");
        }
        assert!(!chat.contains(&"choose_channel"));
        assert!(!chat.contains(&"open_hit"));
        assert!(
            !chat.contains(&"cancel_run"),
            "run cancellation is guest-authored op.submit"
        );
        assert!(
            !chat.contains(&"composer"),
            "message submission belongs to the guest"
        );
    }

    /// NO VIEW HAS AN OPENING ACT OF ITS OWN. Opening a `duck://` address is
    /// the kernel's one door, `host.open_link`, and every seat's route reads
    /// it before the view's own intents ([`host_door`]). A view that grew a
    /// `<module>.open_link` back would be a second grammar for the same act:
    /// two spellings of the address field, two places to scope a link to its
    /// network, and whichever the app decoded last on screen.
    ///
    /// Parsed, not agreed: the view tree is read for the string a notify
    /// would carry, and the door tables for the kind they would admit.
    #[test]
    fn no_view_declares_its_own_open_link() {
        for module in [
            "governance", "members", "agents", "node", "explorer", "call", "chat", "forge",
            "files", "settings", "pages", "home", "inbox",
        ] {
            assert!(
                !intents_of(module).contains(&"open_link"),
                "{module}'s door still admits an open_link of its own"
            );
        }
        let views = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/views");
        let mut sources = Vec::new();
        collect_view_sources(&views, &mut sources);
        assert!(!sources.is_empty(), "the walk found no view source at all");
        for (path, source) in sources {
            // `host.open_link` is the door; anything else before the dot is a
            // module claiming one of its own.
            let own_door = source
                .match_indices(".open_link\"")
                .any(|(at, _)| !source[..at].ends_with("host"));
            assert!(
                !own_door,
                "{} names an `<module>.open_link` — the door is `host.open_link`",
                path.display()
            );
        }
    }

    /// Every `.rs` under `dir`, with its text — source only, never a build
    /// output.
    fn collect_view_sources(
        dir: &std::path::Path,
        out: &mut Vec<(std::path::PathBuf, String)>,
    ) {
        for entry in std::fs::read_dir(dir).expect("read a view source dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some("target") {
                    collect_view_sources(&path, out);
                }
                continue;
            }
            if path.extension().and_then(|end| end.to_str()) == Some("rs") {
                let source = std::fs::read_to_string(&path).expect("read a view source");
                out.push((path, source));
            }
        }
    }

    /// Every kind a module's decoder names is admitted at that module's
    /// door, so a view never emits an intent the app knows how to decode
    /// but refuses to hear. The kinds a decoder names off its door are
    /// exactly the ones that cross by another route. In the other direction the door
    /// admits nothing the decoder leaves to its wildcard, except the kind
    /// the wildcard's verdict itself names.
    #[test]
    fn every_kind_a_decoder_names_is_admitted_at_its_door() {
        use std::collections::BTreeSet;
        let (source, _tests) = include_str!("module_view.rs")
            .split_once("\npub(crate) mod tests {")
            .expect("the tests module");
        let other_route_only: [(&str, &str, &[&str]); 2] = [
            ("settings", "settings_intent", &[]),
            ("chat", "chat_intent", &[]),
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

    #[test]
    fn visibility_subscription_is_generic_and_reports_initial_hidden_state() {
        let _turn = blocking_connection_turn();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/views/chat_view.wasm");
        let mut guest = Guest::load_from("unfamiliar-view", &path).expect("staged view");
        assert!(kernel::answer(&mut guest, "host", "visible", 41, b""));
        assert!(matches!(guest.pending.last(), Some(wire::Event::Response {
            id: 41, result: Ok(bytes), done: false,
        }) if bytes == b"false"));
        guest.pending.clear();
        guest.set_visible(true);
        guest.pending.push(wire::Event::Response {
            id: 99,
            result: Ok(b"queued data".to_vec()),
            done: true,
        });
        guest.sync_visibility();
        assert!(
            matches!(guest.pending.as_slice(), [wire::Event::Response { id: 99, .. }, wire::Event::Response { id: 41, result: Ok(bytes), done: false }] if bytes == b"true")
        );
        guest.pending.clear();
        guest.set_visible(true);
        guest.sync_visibility();
        assert!(guest.pending.is_empty(), "unchanged visibility is silent");
        guest.set_visible(false);
        guest.sync_visibility();
        assert!(
            matches!(guest.pending.last(), Some(wire::Event::Response { id: 41, result: Ok(bytes), done: false }) if bytes == b"false")
        );
        guest.pending.clear();
        guest.frame.cancels = vec![41];
        guest.staged = true;
        guest.redraw(&None);
        guest.set_visible(true);
        guest.sync_visibility();
        assert!(
            guest.pending.is_empty(),
            "cancelled subscription stays retired"
        );
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
            2,
            "the register follows the governance plane; this node's own seat \
             follows the valset"
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

    /// The `home` component — a registry-listed view with no core, seated
    /// off `module_status` alone — end to end through the host, on the
    /// kernel contract and the generic session props: it boots on the
    /// offline plate, and once the session says connected it opens one
    /// `rpc.live` subscription per plane it draws and reads every card
    /// through the kernel doors (`rpc.status`, `rpc.query`, `rpc.view`,
    /// `files.get`), canned here. Needs `make views`; without the staged
    /// component the test says so and does nothing.
    #[test]
    fn the_staged_home_view_boots_and_reads_status_through_the_kernel() {
        let Some(staged) = staged("home") else {
            return;
        };
        let _turn = blocking_connection_turn();
        can_reads([
            (
                "rpc.status",
                serde_json::json!({
                    "public_key": "8c4fa211deadbeef", "height": 84912, "chain_id": "dev#a1b2c3d4",
                    "operations": { "phase": "validating", "sync": {} }
                }),
            ),
            // one canned answer serves every string-named query: the two
            // valset lists and governance's proposals
            (
                "rpc.query",
                serde_json::json!({
                    "validators": [[0x8c, 0x4f, 0xa2, 0x11, 0xde, 0xad, 0xbe, 0xef]],
                    "residents": [[3], [4]],
                    "proposals": []
                }),
            ),
            (
                "channels",
                serde_json::json!({ "channels": { "channels": [
                    { "id": "general", "name": "general", "head_seq": 12,
                      "archived": false, "voice": false }
                ], "has_more": false } }),
            ),
            ("recent", serde_json::json!({ "runs": [] })),
            ("files.get", serde_json::json!({ "snapshots": [] })),
            (
                "rpc.peers",
                serde_json::json!({ "peers": [
                    { "peer": "f00dbeefcafe", "role": "validator", "connected": true }
                ] }),
            ),
            (
                "rpc.blocks",
                serde_json::json!([
                    { "height": 84912, "hash": "ab".repeat(32), "commit_hash": "",
                      "ops": [{ "op_hash": "cd".repeat(32) }] }
                ]),
            ),
        ]);
        let mut guest = Guest::load_from("home", &staged).expect("the view loads");
        assert_eq!(guest.name, "Home", "the tab is named by the manifest");
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

        // connected on the generic session: the view reads every card
        let spec = registered_view("home", false, true, "dev#a1b2c3d4", "7");
        let session = Some(spec.props);
        guest.redraw(&session);
        let mut planes: Vec<&str> = guest
            .live_subscriptions
            .iter()
            .map(|(_, plane)| plane.as_str())
            .collect();
        planes.sort_unstable();
        assert_eq!(
            planes,
            ["block", "chat", "files", "governance", "runs", "valset"],
            "one `rpc.live` subscription per plane the dashboard draws"
        );
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        for expected in [
            "Validating",
            "block 84,912",
            "8c4fa211",
            "#general",
            "Validator",
            "f00dbeef",
            "Online",
            "1 ops",
        ] {
            assert!(
                shown.iter().any(|text| text == expected),
                "missing {expected:?} in {shown:?}"
            );
        }
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
        assert!(
            !guest.redraw(&session),
            "an unchanged session leaves the view quiet"
        );

        // a block on the chat plane: the live item lands and the view
        // reads its rooms again
        let chat_live = guest
            .live_subscriptions
            .iter()
            .find(|(_, plane)| plane == "chat")
            .map(|(id, _)| *id)
            .expect("the chat plane subscription");
        guest.pending.push(wire::Event::Response {
            id: chat_live,
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
            changes: tokio::sync::watch::channel(()).0,
            slot: Slot::Ready(Box::new(guest)),
            props: None,
            generation: 1,
            hash: None,
            in_flight: false,
            wanted: None,
            tasting: None,
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
    pub(super) fn settle_documents(guest: &mut Guest, props: &Option<Vec<u8>>) {
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

    /// A snapshot the fresh build refuses is state lost, not a deployment
    /// refused. The guest reports a refusal before it builds anything, so the
    /// same instance still initializes and the swap goes through — without
    /// that, every retry offers the same snapshot and the tab runs the old
    /// code until the app restarts.
    #[test]
    fn a_refused_snapshot_leaves_the_instance_able_to_start_clean() {
        let Some(staged) = staged("governance") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let bytes = std::fs::read(&staged).expect("the staged view");
        let component = Guest::compile(&bytes, "refusal").expect("the view compiles");
        let mut guest =
            Guest::instantiate("governance", &component, "refusal").expect("the view instantiates");
        // Well-formed, and tagged with a layout that is not this build's:
        // what a snapshot written before the state moved looks like.
        let foreign = wire::Snapshot {
            schema: "0".repeat(64),
            state: wire::SnapshotValue::Bytes(Vec::new()),
        }
        .encode()
        .expect("a snapshot another build could have written");
        let answered = guest
            .restore(&foreign, "refusal")
            .expect("a refusal is the guest's word, not a trap");
        assert!(
            matches!(answered, Restored::Refused(_)),
            "a foreign layout must be refused, not decoded"
        );
        guest
            .init("refusal")
            .expect("the refused instance starts clean");
        guest.redraw(&None);
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
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

    /// The staged Call view through the host, on the share picker: the rows the
    /// HOST offered come back as the only buttons on screen, and pressing one
    /// leaves the `share` intent carrying THAT ROW'S INDEX — which is the whole
    /// of what the view knows about a share target, since only the host can
    /// enumerate a display or a window.
    ///
    /// This is the seam the Rust-side panel test cannot reach: props in through
    /// the real guest, a press through the real wire, an intent out.
    #[test]
    fn the_staged_call_view_offers_the_hosts_share_rows_and_answers_with_one() {
        let Some(view) = staged("call") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("call", &view).expect("the view loads");
        let picking = Some(
            serde_json::to_vec(&serde_json::json!({"panel": {
                "joined": true, "dark": false, "status": "in the huddle",
                // a stage and a tile the picker has to displace
                "video_live": true, "stage": "image:stage", "tiles": ["image:tile"],
                "share_targets": [
                    "Entire desktop — all 2 screens",
                    "DP-1 (primary) — 2560×1440",
                    "src/video.rs — Neovim",
                ],
            }}))
            .expect("props encode"),
        );
        while guest.redraw(&picking) {}
        let shown = texts(&guest);
        assert!(
            shown.iter().any(|text| text == "Share a screen or a window"),
            "{shown:?}"
        );
        assert!(
            shown.iter().any(|text| text == "src/video.rs — Neovim"),
            "a row wears the host's own label: {shown:?}"
        );
        assert!(
            !shown.iter().any(|text| text == "Mute"),
            "the media toggles yield to the picker: {shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);

        // The middle row: its index is what the host resolves against the list
        // it offered, so an off-by-one here shares the wrong screen.
        guest.pending.push(wire::Event::Message(button_message(
            &guest,
            "DP-1 (primary) — 2560×1440",
        )));
        guest.redraw(&picking);
        assert_eq!(
            std::mem::take(&mut guest.intents),
            [ModuleViewEvent {
                kind: "share".to_owned(),
                detail: "1".to_owned(),
            }]
        );

        // And the way out asks the host to close the picker, never to share.
        guest
            .pending
            .push(wire::Event::Message(button_message(&guest, "Cancel")));
        guest.redraw(&picking);
        assert_eq!(
            std::mem::take(&mut guest.intents),
            [ModuleViewEvent {
                kind: "share_cancel".to_owned(),
                detail: "null".to_owned(),
            }]
        );

        // And once a share is running, the huddle NAMES it — the sharer's own
        // stage is their share, so without this the one person who cannot check
        // what they picked is the one who picked it.
        let sharing = Some(
            serde_json::to_vec(&serde_json::json!({"panel": {
                "joined": true, "sharing": true,
                "sharing_label": "src/video.rs — Neovim",
            }}))
            .expect("props encode"),
        );
        while guest.redraw(&sharing) {}
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text == "Sharing src/video.rs — Neovim"),
            "{shown:?}"
        );
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

    #[test]
    fn common_surfaces_do_not_depend_on_the_module_name() {
        for module in ["unlisted-application", "chat", "files", "forge"] {
            for surface in [
                "markdown",
                "code",
                "picture",
                "artifact_svg",
                "artifact_image",
            ] {
                assert!(surface_allowed(surface), "{module}/{surface}");
            }
            assert!(!surface_allowed("unknown-host-capability"));
        }
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
                "connected": true, "dark": false,
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
            ["block", "valset", "block"],
            "the status facts and the peers sample follow the block plane; \
             this node's standing follows the valset"
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
        assert!(!surface_allowed("page_document"));

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

    #[test]
    fn surfaces_without_a_guest_handler_cannot_emit_product_intents() {
        let Some(staged) = staged("chat") else {
            return;
        };
        let mut guest = Guest::load_from("chat", &staged).expect("the view loads");
        assert!(!surface_allowed("chat_composer"));
        assert!(!surface_allowed("forge_composer"));
        guest.surface_event(None, wire::SurfaceValue::Str("unhandled".into()));
        assert!(guest.pending.is_empty());
        assert!(guest.intents.is_empty());
    }

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
                "desktop_notifications": true, "desktop_notifications_host": "ready",
                "unlocked": true,
                "seat_key": "ab12cd34",
                "account_name": "duck", "network_name": "testnet",
                "connected_rpc": "http://127.0.0.1:1",
                "account_ceremony_phase": "", "account_ceremony_qr": "",
                "account_ceremony_detail": "", "account_ceremony_left": "",
                "settings_key_state": "sealed", "settings_key_path": "/keys/user.key",
                "account_number": "42", "account_exists": true,
                "account_busy": false, "account_ticket": "",
                "update_state": "unavailable", "update_current": "", "update_previous": "",
                "update_staged_display": "", "update_channel": "stable",
                "update_checked": "", "update_note": "", "update_busy": false,
                "tasting": []
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

    /// A node-owned view — a built-in module's, or a registry-listed one —
    /// has one source, its deployment on the connected node: with no node
    /// it is not there yet, and the staged file a desktop view would take
    /// is never opened for it.
    #[test]
    fn a_module_owned_view_never_comes_from_the_staged_file() {
        // a staged file for every one of them, where `views_dir` would look
        let _turn = blocking_connection_turn();
        let staged = tempfile::tempdir().expect("a staging dir");
        let node_owned: Vec<&'static str> = crate::backend::view_source::MODULE_OWNED
            .into_iter()
            .chain(["home"])
            .collect();
        for module in &node_owned {
            std::fs::write(staged.path().join(format!("{module}_view.wasm")), b"\0asm")
                .expect("staged");
        }
        // SAFETY: set under the connection turn, the only one a desktop
        // view — the one reader of this variable — is loaded under;
        // `every_desktop_view_is_asked_at_boot` sets it too, under its own.
        unsafe { std::env::set_var("DUCKTAPE_VIEWS_DIR", staged.path()) };
        for module in node_owned {
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
        assert!(crate::backend::view_source::desktop_owned("settings"));
        assert!(!crate::backend::view_source::desktop_owned("home"));
    }

    /// A load still in flight when the app moves to another node lands
    /// nowhere: the node it was asked of may answer late, and its view is
    /// not the view of the node the app is on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_load_the_previous_node_answers_late_is_not_installed() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::node;
        use module_artifact::{Artifact, ModuleArtifact, ViewArtifact};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let deployment = |asset: &str| {
            Artifact::Module(ModuleArtifact {
                component: vec![1, 2, 3],
                index: None,
                view: Some(ViewArtifact {
                    component: component.clone(),
                    assets: [(asset.to_owned(), b"<svg/>".to_vec())].into(),
                }),
                lanes: Vec::new(),
            })
        };
        let status = |artifact: &Artifact| {
            serde_json::json!({"module_status": {"modules": [
                {"module_id": "forge", "kind": "module", "active_code_hash": artifact.hash().to_vec(),
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
        // CHAT plane, which is where a note it draws is committed, plus this
        // node's own standing on the VALSET plane, which is the word the org
        // head wears
        let planes: std::collections::BTreeSet<&str> = guest
            .live_subscriptions
            .iter()
            .map(|(_, plane)| plane.as_str())
            .collect();
        assert_eq!(
            planes,
            ["chat", "forge", "valset"].into_iter().collect(),
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

        for surface in ["picture", "markdown", "code"] {
            assert!(
                surface_allowed(surface),
                "the host paints the {surface} slot the view leaves"
            );
        }
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// A deployment of `module` on the fake node: the staged governance
    /// component as its view, told apart by the one asset it ships.
    fn deployment(component: &[u8], asset: &str) -> module_artifact::Artifact {
        module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: Some(module_artifact::ViewArtifact {
                component: component.to_vec(),
                assets: [(asset.to_owned(), b"<svg/>".to_vec())].into(),
            }),
            lanes: Vec::new(),
        })
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
        // and the previous test's taste set, with the frames it held
        taste::clear();
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
            changes: tokio::sync::watch::channel(()).0,
            slot: Slot::Ready(Box::new(guest)),
            props: Some(b"previous test props".to_vec()),
            generation: 9,
            hash: Some([7; 32]),
            in_flight: false,
            wanted: None,
            tasting: None,
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

    /// The session facts a kernel-contract view is pushed: connected, in
    /// this colour, with the taste set. Whether this node may act is the
    /// VIEW's own read of the valset, so it is not here.
    fn session_props() -> Option<Vec<u8>> {
        Some(
            serde_json::to_vec(&serde_json::json!({
                "connected": true, "dark": false, "tasting": []
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_registered_view_reuses_http_while_its_registry_thread_joins_it() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let component = std::fs::read(staged("governance").expect("build the governance view"))
            .expect("the staged view");
        let home = module_artifact::Artifact::View(module_artifact::ViewArtifact {
            component,
            assets: std::collections::BTreeMap::new(),
        });
        let node = FakeDeployment::serving("home", &home);
        node.status.lock().unwrap()["module_status"]["modules"][0]["kind"] =
            serde_json::json!("view");
        node.answer_query("governance", serde_json::json!({"proposals": []}));
        let client = fake_node(node).await;
        // Only the registry talks first, so its HTTP pool is the one home
        // inherits. Its loader thread then waits for home's nested load.
        let asked_of = {
            let mut current = connection().lock().unwrap();
            current.rev += 1;
            current.client = Some(client);
            current.clone()
        };
        let loading = spawn_registry_read(asked_of);
        tokio::task::spawn_blocking(move || loading.join().unwrap())
            .await
            .unwrap();
        assert_eq!(registered_views(), ["home"]);
        assert_eq!(canary::seated_hash("home"), Some(home.hash()));
        assert!(matches!(
            mounted("home").lock().unwrap().slot,
            Slot::Ready(_)
        ));
    }

    /// The connect seats and loads every node-owned view, drawn or not —
    /// the built-in module tabs, and every `Kind::View` entry the node's
    /// registry lists, found off the registry itself — and a draw before
    /// any node asks for nothing. A registered view's tab is named by its
    /// manifest and iconed by its `icons/tab.svg`; a block that retires the
    /// entry takes the seat and the tab with it.
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
        // `home`: a view-only entry whose frame IS the view — the same
        // staged component, under its own id, with a tab icon beside it
        let home = module_artifact::Artifact::View(module_artifact::ViewArtifact {
            component: component.clone(),
            assets: [("icons/tab.svg".to_owned(), b"<svg/>".to_vec())].into(),
        });
        let node = FakeDeployment::serving("governance", &a);
        let listing = |with_home: bool| {
            let mut modules = vec![serde_json::json!({
                "module_id": "governance", "kind": "module", "active_code_hash": a.hash(),
                "pending": null, "history": [{"height": 7, "code_hash": a.hash()}]
            })];
            if with_home {
                modules.push(serde_json::json!({
                    "module_id": "home", "kind": "view", "active_code_hash": home.hash(),
                    "pending": null, "history": [{"height": 0, "code_hash": home.hash()}]
                }));
            }
            serde_json::json!({"module_status": {"modules": modules}})
        };
        *node.status.lock().unwrap() = listing(true);
        node.artifacts.lock().unwrap().push(home.clone());
        let client = fake_node(node.clone()).await;
        // A presenter asks for a seat before any node: nothing starts loading.
        drop(mounted("files"));
        {
            let seat = mounted("files");
            let locked = seat.lock().expect("module view lock");
            assert!(matches!(locked.slot, Slot::Loading));
            assert!(!locked.in_flight, "the draw started a load");
            assert_eq!(locked.generation, 0);
        }
        assert!(registered_views().is_empty(), "no registry read yet");
        let loads = connected(&client);
        assert_eq!(
            loads.started(),
            MODULE_OWNED.len() + 1,
            "one load per built-in module view, drawn or not, plus the registry read"
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
        // the registry's view entry is seated off the connect too, and its
        // tab is drawn from what it ships
        assert_eq!(registered_views(), ["home"]);
        assert_eq!(slot_assets(&mounted("home")), ["icons/tab.svg"]);
        assert_eq!(registered_view_name("home"), "Approvals");
        assert_eq!(
            registered_view_icon("home").as_deref(),
            Some(&b"<svg/>".to_vec())
        );
        assert_eq!(canary::seated_hash("home"), Some(home.hash()));

        // a block that retires the entry: the seat and the tab go
        *node.status.lock().unwrap() = listing(false);
        deployments_check().await.joined();
        assert!(
            registered_views().is_empty(),
            "a retired view's tab is gone"
        );
        assert!(
            !registry()
                .lock()
                .expect("module views")
                .contains_key("home"),
            "a retired view's seat is gone"
        );
        // and one that lists it again seats it again
        *node.status.lock().unwrap() = listing(true);
        deployments_check().await.joined();
        assert_eq!(registered_views(), ["home"]);
        assert_eq!(canary::seated_hash("home"), Some(home.hash()));
        for module in MODULE_OWNED.into_iter().chain(["home"]) {
            registry().lock().expect("module views").remove(module);
        }
        registered().lock().expect("registered views").clear();
        registered_modules().lock().expect("registered modules").clear();
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
    /// callers of `spawn_load` are the boot, the connect, the block check
    /// (the registry-listed seats they share, `seat_registered_views`, is
    /// called by the connect's registry read and the block check alone),
    /// and a taste — the member's own act (`retaste`) or the connect's
    /// registry read seating what the device remembered
    /// (`seat_remembered_tastes`). The views the shell draws by name are
    /// exactly the ones those ask for — a registered view is drawn by the
    /// id the registry listed.
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
            BTreeSet::from([
                "booted",
                "connected",
                "deployments_check",
                "retaste",
                "seat_registered_views",
                "seat_remembered_tastes",
            ]),
            "a load started outside the boot, the connect, the block check and a taste"
        );
        let registered_callers: BTreeSet<&str> = shell
            .lines()
            .filter(|line| line.contains("seat_registered_views(&mut registry"))
            .map(str::trim)
            .collect();
        assert_eq!(
            registered_callers.len(),
            2,
            "the registered seats are made by the connect's registry read and the block check: {registered_callers:?}"
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
                "account": "7", "route": "", "route_serial": 0
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
        // The frame the guest raises while the press is in flight: its own
        // tick answers the request and redraws the room under it.
        let later = {
            let mut later = root.clone();
            let wire::Node::Linear { children, .. } = &mut later else {
                unreachable!()
            };
            children.push(input("arrived"));
            later
        };
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
                // Every command here is queued against one frame and
                // performed after the next one landed — the sequence every
                // press takes, and the one that used to refuse them all.
                guest.frame.root = Some(later.clone());
                guest.frame_rev += 1;
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
    fn widget_requests_validate_scope_payload_budget_and_target() {
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
        assert!(!guest.widget_commands.iter().any(|(id, _)| *id == 0));
        // A frame the guest raised under the queued presses keeps their
        // target mounted, so every one of them still runs.
        guest.frame_rev += 1;
        let mut performed = 0;
        guest.execute_widget_commands(|_| {
            performed += 1;
            Ok(Vec::new())
        });
        assert_eq!(performed, MAX_REQUESTS_PER_TICK - 1, "one id was cancelled");
        assert!(guest.widget_commands.is_empty());
        assert!(
            guest
                .pending
                .drain(..)
                .all(|event| matches!(event, wire::Event::Response { result: Ok(_), .. }))
        );
        // The target itself leaving is the only thing that drops one.
        guest.answer(request(1, wire::encode(&command)), &None);
        guest.frame.root = Some(wire::Node::empty());
        guest.frame_rev += 1;
        guest.execute_widget_commands(|_| panic!("a vanished target must not touch native state"));
        assert!(matches!(
            guest.pending.pop(),
            Some(wire::Event::Response { result: Err(error), .. }) if error == "widget target left the tree"
        ));
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
    pub(super) fn pages_facts() -> Option<Vec<u8>> {
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
    pub(super) fn can_a_commented_page() {
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
                    if let wire::Node::Container { key, max_width, .. } = node
                        && key == "pages/document/surface"
                    {
                        published_width = *max_width;
                    }
                });
                assert_eq!(
                    published_width,
                    Some(pane_width - 320. - 24.),
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
        assert_eq!(f32::from(beside_editor.size.width), 670.);
        assert_eq!(f32::from(beside_card.size.width), 320.);
        let (squeeze_editor, squeeze_card) = measured(1300.);
        assert_eq!(
            f32::from(squeeze_editor.size.width),
            1066. - 320. - 24. - 96.
        );
        assert_eq!(f32::from(squeeze_card.size.width), 320.);
        assert!(squeeze_editor.size.width < beside_editor.size.width);
        // 1000 − 234 = 766 of pane: under the 844 squeeze floor, so inline.
        let (inline_editor, inline_card) = measured(1000.);
        assert_eq!(f32::from(inline_editor.size.width), 670.);
        assert_eq!(inline_card.size.width, inline_editor.size.width);
    }

    /// The forge view draws itself against session facts only; everything
    /// on its screen it reads for itself over the kernel contract.
    fn forge_facts() -> Option<Vec<u8>> {
        Some(
            br#"{
          "dark": false, "connected": true, "org": "duckhouse", "about": "",
          "network_chain_id": "mynet#d0cdf950",
          "connected_rpc": "http://127.0.0.1:1", "link": "", "link_tick": 0
        }"#
            .to_vec(),
        )
    }

    pub(super) fn chat_facts() -> Option<Vec<u8>> {
        chat_facts_in("channel-a", 0)
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
    /// messages off the index, so what the app pushes is who the reader is
    /// and which room she is in. The runs in flight are not among them: the
    /// view discovers and folds those itself.
    fn chat_facts_in(room: &'static str, land_seq: i64) -> Option<Vec<u8>> {
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
            active_channel: room,
            dm_peer: "",
            dm_serial: 0,
            land_seq,
            busy: false,
            loading: false,
            huddle_joined: false,
            huddle_channel: "",
            huddle_channel_name: "",
            huddle_joined_at: 0,
            huddle_now: 0,
            call_muted: false,
            call_speaking: false,
            call_peers: &[],
            shift_held: false,
            copy_chord_serial: 0,
        };
        Some(serde_json::to_vec(&props).expect("props encode"))
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
        can_the_chat_room();
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

    /// A proposed frame with the ACTIVE core and another view: the one
    /// shape a member may taste.
    fn proposed_view(component: &[u8], asset: &str) -> module_artifact::Artifact {
        deployment(component, asset)
    }

    /// The governance tab's seat over a node serving `active`, holding
    /// `staged` (the bytes proposals fanned out) and listing one open code
    /// ballot per pair in `open`; the staged governance view drawn and
    /// settled on the session facts, its register read.
    async fn seated_over(
        active: &module_artifact::Artifact,
        staged: &[&module_artifact::Artifact],
        open: &[(&str, [u8; 32])],
    ) -> (
        Arc<crate::backend::view_source::tests::FakeDeployment>,
        ducktape_rpc::Client,
        Arc<Mutex<Mounted>>,
    ) {
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let node = FakeDeployment::serving("governance", active);
        for artifact in staged {
            node.stage(artifact);
        }
        node.propose(open);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        {
            let mut locked = mounted.lock().expect("module view lock");
            let Slot::Ready(guest) = &mut locked.slot else {
                panic!("the view of the active deployment");
            };
            settle_documents(guest, &session_props());
        }
        (node, client, mounted)
    }

    /// The seated governance view redrawn on the session — the taste set
    /// in its props — and settled; its texts.
    fn redrawn_texts(mounted: &Arc<Mutex<Mounted>>) -> Vec<String> {
        // the props read the seat's taste under its own lock, as the
        // shell's render does: built before this seat is held
        let props = Some(governance_view(false, true).props);
        let mut locked = mounted.lock().expect("module view lock");
        let Slot::Ready(guest) = &mut locked.slot else {
            panic!("a seated view");
        };
        settle_documents(guest, &props);
        texts(guest)
    }

    /// A member tastes the view an open proposal would install: the seat
    /// loads the proposed frame's view in place of the active one, the tab
    /// says so, the preference is kept for the chain, and a block that
    /// finds the taste still open leaves the seat alone. Back and forth is
    /// a swap out of the frames held, not a fetch. When the proposal is
    /// withdrawn the seat returns to the active view with a notice, and the
    /// preference is forgotten.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_member_tastes_an_open_proposals_view_and_is_returned_when_it_is_withdrawn() {
        let _turn = connection_turn().await;
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            proposed_view(&component, "b.svg"),
        );
        // the ballot is open and the bytes were fanned out: the connect's
        // walk lists the pair, and the card offers it — nobody is forced
        let (node, _client, mounted) = seated_over(&a, &[&b], &[("governance", b.hash())]).await;
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        let rows = taste::rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].module, rows[0].hash, rows[0].refusal, rows[0].stage),
            (
                "governance",
                b.hash(),
                None,
                crate::backend::view_source::Stage::Open
            )
        );
        assert_eq!(rows[0].proposal.as_deref(), Some("prop-0"));
        let props = taste_props();
        assert_eq!(props[0]["reason"], "");
        assert_eq!(props[0]["tasting"], false);
        assert_eq!(props[0]["name"], "Governance");
        assert_eq!(props[0]["status"], "open");
        let shown = redrawn_texts(&mounted);
        assert!(
            shown.iter().any(|text| text == "Try this view"),
            "the card offers the taste: {shown:?}"
        );

        // the member tries it
        let generation = mounted.lock().expect("module view lock").generation;
        taste("governance", b.hash()).joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        {
            let locked = mounted.lock().expect("module view lock");
            assert_eq!(locked.tasting, Some(b.hash()));
            assert_eq!(locked.hash, Some(b.hash()));
            assert!(locked.generation > generation);
        }
        assert_eq!(
            tab_label("governance", "Governance"),
            "Governance · proposed"
        );
        assert_eq!(
            taste::remembered("fake-chain"),
            [("governance".to_owned(), b.hash())].into()
        );
        assert_eq!(taste_props()[0]["tasting"], true);
        let shown = redrawn_texts(&mounted);
        assert!(
            shown.iter().any(|text| text == "Back to current"),
            "the card offers the way back: {shown:?}"
        );
        // a block that finds the taste still open leaves the seat alone
        let generation = mounted.lock().expect("module view lock").generation;
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        assert_eq!(
            mounted.lock().expect("module view lock").generation,
            generation
        );
        // and the same taste again is nothing to do
        assert_eq!(taste("governance", b.hash()).started(), 0);

        // back to the current view, and to the taste again: the frames
        // are held, so neither asks the node for bytes
        node.artifacts.lock().unwrap().clear();
        untaste("governance").joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        assert_eq!(mounted.lock().expect("module view lock").tasting, None);
        assert!(taste::remembered("fake-chain").is_empty());
        assert_eq!(tab_label("governance", "Governance"), "Governance");
        taste("governance", b.hash()).joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        assert!(
            taste::take_notices().is_empty(),
            "a member's own move is no news"
        );

        // the ballot is withdrawn: the next block returns the seat
        node.propose(&[]);
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        {
            let locked = mounted.lock().expect("module view lock");
            assert_eq!(locked.tasting, None);
            assert_eq!(locked.hash, Some(a.hash()));
        }
        assert_eq!(
            taste::take_notices(),
            ["The proposed Governance view was withdrawn — back to the current view"]
        );
        assert!(taste::remembered("fake-chain").is_empty());
        assert!(taste::rows().is_empty());
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// A taste that the ballot passes stays a taste while the swap is
    /// scheduled — the card reads "scheduled" — and when the swap
    /// activates the tasted hash IS the active one: the seat is not
    /// reloaded, the taste is cleared, and the member is told.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_activated_taste_is_the_current_view_without_a_reload() {
        let _turn = connection_turn().await;
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            proposed_view(&component, "b.svg"),
        );
        let (node, client, mounted) = seated_over(&a, &[&b], &[("governance", b.hash())]).await;
        taste("governance", b.hash()).joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        // passed: the registry schedules the swap, the ballot settles
        node.schedule("governance", &a, &b, 40);
        node.propose(&[]);
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        assert_eq!(
            mounted.lock().expect("module view lock").tasting,
            Some(b.hash())
        );
        let props = taste_props();
        assert_eq!(props[0]["status"], "scheduled");
        assert_eq!(props[0]["activation_height"], 40);
        assert_eq!(props[0]["tasting"], true);
        assert!(taste::take_notices().is_empty());
        // the height: B is active, nothing is pending, nothing is open
        let generation = mounted.lock().expect("module view lock").generation;
        node.deploy("governance", &b);
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        {
            let locked = mounted.lock().expect("module view lock");
            assert_eq!(locked.tasting, None);
            assert_eq!(locked.hash, Some(b.hash()));
            assert_eq!(
                locked.generation, generation,
                "no reload: the seat already draws it"
            );
        }
        assert_eq!(
            taste::take_notices(),
            ["The Governance view you were trying is now the current view"]
        );
        assert!(taste::remembered("fake-chain").is_empty());
        assert_eq!(tab_label("governance", "Governance"), "Governance");
        drop(client);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// Every refusal by name: a proposal that changes the core too, one
    /// whose bytes the node lacks (until it holds them), one that removes
    /// the view, one of another kind, and one whose view does not speak
    /// this app's wire. None is ever seated, and the card says why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_proposal_is_listed_by_reason_and_never_seated() {
        let _turn = connection_turn().await;
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let core_changes = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
            component: vec![9, 9, 9],
            index: None,
            view: Some(module_artifact::ViewArtifact {
                component: component.clone(),
                assets: [("core.svg".to_owned(), b"<svg/>".to_vec())].into(),
            }),
            lanes: Vec::new(),
        });
        let no_view = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: None,
            lanes: Vec::new(),
        });
        let view_only = module_artifact::Artifact::View(module_artifact::ViewArtifact {
            component: component.clone(),
            assets: Default::default(),
        });
        let other_wire = proposed_view(b"not a component", "wire.svg");
        let not_held = proposed_view(&component, "held.svg");
        let (node, _client, mounted) = seated_over(
            &a,
            &[&core_changes, &no_view, &view_only, &other_wire],
            &[
                ("governance", core_changes.hash()),
                ("governance", no_view.hash()),
                ("governance", view_only.hash()),
                ("governance", other_wire.hash()),
                ("governance", not_held.hash()),
            ],
        )
        .await;
        let reason_of = |hash: [u8; 32]| {
            taste::rows()
                .into_iter()
                .find(|row| row.hash == hash)
                .and_then(|row| row.refusal)
                .map(taste::Refusal::reason)
        };
        assert_eq!(reason_of(core_changes.hash()), Some("core_changes_too"));
        assert_eq!(reason_of(no_view.hash()), Some("no_view"));
        assert_eq!(reason_of(view_only.hash()), Some("kind_mismatch"));
        assert_eq!(reason_of(other_wire.hash()), Some("wire_protocol"));
        assert_eq!(reason_of(not_held.hash()), Some("not_held"));
        for refused in [&core_changes, &no_view, &view_only, &other_wire, &not_held] {
            assert_eq!(taste("governance", refused.hash()).started(), 0);
            assert_eq!(slot_assets(&mounted), ["a.svg"]);
            assert_eq!(mounted.lock().expect("module view lock").tasting, None);
        }
        assert!(taste::remembered("fake-chain").is_empty());
        let props = taste_props();
        let reasons: Vec<&str> = props
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["reason"].as_str().unwrap())
            .collect();
        assert_eq!(reasons.len(), 5);
        assert!(reasons.iter().all(|reason| !reason.is_empty()));
        let shown = redrawn_texts(&mounted);
        for expected in [
            "Changes the module's code too — it becomes current when it activates",
            "Removes the view",
            "Your node has not received these bytes yet",
        ] {
            assert!(
                shown.iter().any(|text| text == expected),
                "missing {expected:?} in {shown:?}"
            );
        }
        assert!(
            !shown.iter().any(|text| text == "Try this view"),
            "{shown:?}"
        );
        // the bytes arrive: the next walk finds them
        node.stage(&not_held);
        deployments_checked().await.joined();
        assert_eq!(reason_of(not_held.hash()), None);
        taste("governance", not_held.hash()).joined();
        assert_eq!(slot_assets(&mounted), ["held.svg"]);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// A taste is a device preference: at connect, once the chain's
    /// registry has answered, the remembered hash is seated again — if the
    /// taste set still names it — and a taste the set no longer names is
    /// left where it was.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remembered_taste_is_seated_again_at_connect() {
        let _turn = connection_turn().await;
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            proposed_view(&component, "b.svg"),
        );
        let (node, client, mounted) = seated_over(&a, &[&b], &[("governance", b.hash())]).await;
        taste("governance", b.hash()).joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        // the app reconnects: the seat is asked of the node afresh, and
        // the remembered taste rides in once the registry has answered
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        assert_eq!(
            mounted.lock().expect("module view lock").tasting,
            Some(b.hash())
        );
        // the proposal is gone before the next connect: the preference
        // names nothing the set does, and the seat draws the active view
        node.propose(&[]);
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        assert_eq!(mounted.lock().expect("module view lock").tasting, None);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
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
        let removed = module_artifact::Artifact::module(vec![9, 9, 9]);
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
        let removed = module_artifact::Artifact::module(vec![9, 9, 9]);
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

    #[test]
    fn asset_reads_use_only_the_requesting_guests_current_deployment() {
        let Some(staged) = staged("pages") else {
            return;
        };
        let mut first = Guest::load_from("pages", &staged).expect("first guest");
        let mut second = Guest::load_from("pages", &staged).expect("second guest");
        let assets = |bytes: &[u8]| Arc::new([("service.json".to_owned(), bytes.to_vec())].into());
        first.deployed([1; 32], assets(b"first"));
        second.deployed([2; 32], assets(b"second"));
        let read = |guest: &mut Guest, path: &[u8]| {
            assert!(kernel::answer(guest, "asset", "read", 17, path));
            let wire::Event::Response { id, result, done } =
                guest.pending.pop().expect("asset answer")
            else {
                panic!("response")
            };
            assert_eq!(id, 17);
            assert!(done);
            result
        };
        assert_eq!(read(&mut first, b"service.json").unwrap(), b"first");
        assert_eq!(read(&mut second, b"service.json").unwrap(), b"second");
        first.deployed([3; 32], assets(b"replacement"));
        assert_eq!(read(&mut first, b"service.json").unwrap(), b"replacement");
        assert_eq!(read(&mut second, b"service.json").unwrap(), b"second");
        for path in [
            b"../service.json".as_slice(),
            b"/service.json",
            b"./service.json",
            b"https://example.test/service.json",
            b"missing",
            &[255],
        ] {
            assert!(read(&mut first, path).is_err(), "{path:?}");
        }
        first.deployed([4; 32], assets(&vec![0; (1 << 20) + 1]));
        assert!(read(&mut first, b"service.json").is_err());
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
            let manifest = view_wire::manifest::read_manifest(&bytes).expect("a manifest");
            assert_eq!(manifest.name, "Approvals");
            assert!(
                view_wire::manifest::read_manifest(b"\0asm\x01\0\0\0").is_none(),
                "a bare core module has no manifest"
            );
        }
    }

    /// Picture storage refuses an absent surface identity.
    #[test]
    fn the_kernel_stores_a_picture_under_its_surface_identity() {
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

    /// The name a line binds the connection guard to, if what it binds IS
    /// the guard: a read that projects off it (`.rev`, `.clone()`) lets the
    /// guard go at the end of its own statement and holds nothing.
    fn bound_connection_guard(line: &str) -> Option<&str> {
        let (before, after) = line.split_once("connection().lock()")?;
        let (name, _) = before.trim().strip_prefix("let ")?.split_once('=')?;
        let name = name.trim().trim_start_matches("mut ").trim();
        let tail = after.trim();
        let tail = match tail.strip_prefix(".expect(") {
            Some(rest) => rest.split_once(')').map_or(rest, |(_, rest)| rest),
            None => tail.strip_prefix(".unwrap()").unwrap_or(tail),
        };
        (tail.trim() == ";").then_some(name)
    }

    /// Every place a source binds the connection guard and takes another
    /// lock before that binding goes out of scope.
    fn guards_spanning_another_lock(path: &str, source: &str) -> Vec<String> {
        let lines: Vec<&str> = source.lines().collect();
        let mut found = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let Some(name) = bound_connection_guard(line) else {
                continue;
            };
            let mut depth = 0i32;
            for (offset, inside) in lines.iter().enumerate().skip(index + 1) {
                depth += inside.matches('{').count() as i32;
                depth -= inside.matches('}').count() as i32;
                let scope_ended = depth < 0 || inside.contains(&format!("drop({name})"));
                if scope_ended {
                    break;
                }
                // `mounted` takes the registry's lock before it hands a seat over
                let takes_another_lock =
                    inside.contains(".lock()") || inside.contains("mounted(module)");
                if takes_another_lock {
                    found.push(format!(
                        "{path}:{} binds `{name}`, and {path}:{} locks under it",
                        index + 1,
                        offset + 1
                    ));
                    break;
                }
            }
        }
        found
    }

    /// THE CONNECTION LOCK IS A LEAF. The window thread draws under a
    /// seat's lock, and a guest call inside that redraw asks
    /// [`connection`] for its client — so a thread that holds the
    /// connection while it waits for a seat, or for the registry the seats
    /// live in, closes the cycle and the app never paints again. The shape
    /// is the invariant, so the source is what states it.
    #[test]
    fn the_connection_guard_never_spans_another_lock() {
        let held: Vec<String> = [
            ("app/src/module_view.rs", include_str!("module_view.rs")),
            (
                "app/src/module_view/background.rs",
                include_str!("module_view/background.rs"),
            ),
            (
                "app/src/module_view/kernel.rs",
                include_str!("module_view/kernel.rs"),
            ),
        ]
        .into_iter()
        .flat_map(|(path, source)| guards_spanning_another_lock(path, source))
        .collect();
        assert!(
            held.is_empty(),
            "the connection lock is a leaf; these hold it across another:\n{}",
            held.join("\n")
        );
    }
}

#[cfg(test)]
mod extension_tests;
