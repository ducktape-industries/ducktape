//! NATIVE DESKTOP NOTIFICATIONS for the two arrivals a person cannot afford to
//! find later: being `@mentioned`, and a direct message.
//!
//! Everything a mention produced before this landed inside the app — an inbox
//! row and the bell's unread count — which is worth nothing to a reader whose
//! window is behind an editor. The decision of WHETHER to notify is pure and
//! lives in [`desktop_notice`]; posting is one call on whichever notifier the
//! host offers, and only that call is untestable.
//!
//! "Behind an editor" is the app's own fact on every host: the window focus
//! events say whether any window of this app has focus, and the reducer
//! records their answer here ([`note_window_focus`]) for the live fold, which
//! runs beside the reducer and cannot read its state.
//!
//! THE NOTIFIER IS WHAT THE HOST OFFERS. On macOS it is the notification
//! center, which terminates a process that has no bundle identifier — and
//! `cargo test` is exactly such a process — so the bundle is checked before
//! the framework is touched, and a bare binary (`make dev`) says so ONCE at
//! boot and stays silent. Everywhere else it is the freedesktop notifications
//! service on the session bus; a host with no session bus degrades the same
//! way.
//!
//! THE HOST IS ASKED AT BOOT, not at the first mention. macOS prompts the
//! person once per bundle id and stores the answer; a request submitted while
//! the prompt is still up is refused (`UNErrorCodeNotificationsNotAllowed`),
//! and a stored "Don't Allow" refuses every request after it. Asking in
//! [`boot_desktop_notifications`] puts the prompt at launch, where the person expects it, and the
//! answer where they can read it: one log line per boot and the Settings tab
//! ([`desktop_notifications_host`]), which names what to do about a refusal.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// How much of a message body a notification carries. A banner shows two or
/// three lines; past that the excerpt is only paying for itself in memory.
const EXCERPT_CHARS: usize = 140;

/// One notification, already worded — the shape the platform call takes.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

// ============================================================================
// the preference
// ============================================================================

/// The prefs key. DEVICE-global like `appearance`: whether this machine may
/// raise a banner is a property of the machine, not of a workspace.
const NOTIFY_PREF: &str = "desktop_notifications";

/// Default ON — a person who installs a chat app expects to be told they were
/// named. Only an explicit `false` turns it off.
pub fn notifications_enabled() -> bool {
    read_prefs()[NOTIFY_PREF].as_bool().unwrap_or(true)
}

/// The Settings toggle's reading, at boot.
pub async fn load_desktop_notifications() -> bool {
    notifications_enabled()
}

// ============================================================================
// what the host answered
// ============================================================================

/// WHETHER THIS HOST WILL RAISE A BANNER, as it last told us. One
/// discriminant: the Settings tab draws one sentence per variant, and a new
/// answer has to be routed there rather than folded into a boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HostNotifier {
    /// asked, not yet answered — the prompt may be on screen
    Pending = 0,
    /// the host will show them
    Ready = 1,
    /// this process is not an app bundle (`make dev`): macOS attributes a
    /// banner to a bundle id and refuses a process without one
    Unbundled = 2,
    /// the person (or a past answer) refused, in System Settings
    Denied = 3,
    /// no notification service on this desktop (no session bus)
    Unavailable = 4,
}

impl HostNotifier {
    /// The wire token the Settings view receives. Stable: the view matches on
    /// it.
    pub fn token(self) -> &'static str {
        match self {
            HostNotifier::Pending => "pending",
            HostNotifier::Ready => "ready",
            HostNotifier::Unbundled => "unbundled",
            HostNotifier::Denied => "denied",
            HostNotifier::Unavailable => "unavailable",
        }
    }

    fn from_u8(value: u8) -> HostNotifier {
        match value {
            1 => HostNotifier::Ready,
            2 => HostNotifier::Unbundled,
            3 => HostNotifier::Denied,
            4 => HostNotifier::Unavailable,
            _ => HostNotifier::Pending,
        }
    }
}

/// The host's latest answer. Written by the boot request and by every post's
/// completion, both of which land on framework threads; read by the Settings
/// props on each draw.
static HOST: AtomicU8 = AtomicU8::new(HostNotifier::Pending as u8);

fn record_host(answer: HostNotifier) {
    HOST.store(answer as u8, Ordering::Relaxed);
}

/// What the host last said about raising banners.
pub fn desktop_notifications_host() -> HostNotifier {
    HostNotifier::from_u8(HOST.load(Ordering::Relaxed))
}

/// ASK THE HOST ONCE, AT LAUNCH. Requests authorization where the platform
/// needs it and logs the answer once per boot; the answer is then readable
/// through [`desktop_notifications_host`]. Never blocks: the platform answers on its own
/// thread.
pub fn boot_desktop_notifications() {
    platform::boot();
}

/// Persist it. Best-effort like `save_appearance`: a failed write costs the
/// NEXT boot's default and nothing this session shows.
pub async fn save_desktop_notifications(enabled: bool) -> bool {
    let mut prefs = read_prefs();
    prefs[NOTIFY_PREF] = serde_json::json!(enabled);
    write_prefs(&prefs)
}

// ============================================================================
// what the live fold cannot see for itself
// ============================================================================

/// THE ROOM ON SCREEN, recorded where it is decided. The live decoder runs off
/// to the side of the reducer and is handed one op, so the two facts a
/// suppression needs — which room is open, and what the rooms are called —
/// reach it the way `ACCOUNT_NAMES` does: recorded by the load that learns
/// them.
static ACTIVE_CHANNEL: RwLock<String> = RwLock::new(String::new());
/// channel id → the name the sidebar draws.
static ROOM_NAMES: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());
/// the two-party rooms THIS reader is a party to.
static MY_DM_ROOMS: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Record the rooms and the one the reader is in — called by the chat load,
/// which is what decides both.
pub(crate) fn note_rooms(channels: &[ChatChannel], active: &str) {
    if let Ok(mut names) = ROOM_NAMES.write() {
        for channel in channels {
            names.insert(channel.id.clone(), format!("#{}", channel.name));
        }
    }
    if let Ok(mut open) = ACTIVE_CHANNEL.write() {
        active.clone_into(&mut open);
    }
}

/// Record this reader's own DM rooms and what to call them — the directory
/// load already derives both (`DmPeer.channel_id`, `DmPeer.name`).
pub(crate) fn note_dm_rooms(peers: &[DmPeer]) {
    let mine: BTreeSet<String> = peers
        .iter()
        .map(|peer| peer.channel_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    if let Ok(mut names) = ROOM_NAMES.write() {
        for peer in peers.iter().filter(|peer| !peer.channel_id.is_empty()) {
            names.insert(peer.channel_id.clone(), peer.name.clone());
        }
    }
    if let Ok(mut rooms) = MY_DM_ROOMS.write() {
        *rooms = mine;
    }
}

fn room_name(channel_id: &str) -> String {
    ROOM_NAMES
        .read()
        .ok()
        .and_then(|names| names.get(channel_id).cloned())
        .unwrap_or_else(|| channel_id.to_string())
}

fn in_my_dm(channel_id: &str) -> bool {
    MY_DM_ROOMS
        .read()
        .is_ok_and(|rooms| rooms.contains(channel_id))
}

fn active_channel() -> String {
    ACTIVE_CHANNEL
        .read()
        .map(|open| open.clone())
        .unwrap_or_default()
}

/// WHETHER ANY WINDOW OF THIS APP HAS FOCUS, as the OS last reported it. A
/// banner is for a reader who is elsewhere; a mention landing in the window
/// they are looking at is already on screen.
static APP_FOCUSED: AtomicBool = AtomicBool::new(false);

/// Record the focus the window events reported: `true` when a window of this
/// app took focus, `false` when the last focused one lost it. A task so the
/// reducer can call it where it learns the fact; it has nothing to deliver.
pub fn note_window_focus(focused: bool) -> ducktape_view_guest::Task<()> {
    APP_FOCUSED.store(focused, Ordering::Relaxed);
    ducktape_view_guest::Task::none()
}

fn app_focused() -> bool {
    APP_FOCUSED.load(Ordering::Relaxed)
}

// ============================================================================
// the live trigger
// ============================================================================

/// One applied chat op → at most one banner. Called from the live decoder with
/// the payload it already decoded the delta from.
///
/// Only a POST notifies: an edit, a reaction or a membership change is not an
/// arrival. Everything this reads is either in the op or in a warm cache — no
/// query runs here, for the reason the decoder's other reads are cached
/// (a `/v1/query` inside the fold freezes every subscriber).
pub(crate) fn notify_chat_op(
    payload: &[u8],
    assigned: Option<&serde_json::Value>,
    names: &NameDirectory,
) {
    let me = rpc::cached_user_key();
    let Some(arrival) = chat_arrival(payload, assigned, names, me.as_deref()) else {
        return;
    };
    let screen = OnScreen {
        app_focused: app_focused(),
        active_channel: active_channel(),
    };
    let Some(notice) = desktop_notice(&arrival, notifications_enabled(), &screen) else {
        return;
    };
    platform::post(&notice);
}

/// How many posts the host has refused this boot. The first refusal is a
/// `warn!`; the rest are `debug!` carrying this count, so a standing refusal
/// is one line in the ring and not a line per mention.
fn count_refusal() -> u64 {
    use std::sync::atomic::AtomicU64;
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    REFUSED.fetch_add(1, Ordering::Relaxed) + 1
}

/// Notification identity comes from the source's canonical post assignment,
/// including its account-resolved mentions, just like the settled timeline.
pub(crate) fn chat_arrival(
    payload: &[u8],
    assigned: Option<&serde_json::Value>,
    names: &NameDirectory,
    me: Option<&[u8]>,
) -> Option<Arrival> {
    let ::chat::ChatMsg::PostMessage {
        channel_id, blocks, ..
    } = ::chat::decode_msg(payload).ok()?
    else {
        return None;
    };
    let ::chat::ChatAssigned::Posted {
        actor,
        key_mentions,
        ..
    } = serde_json::from_value(assigned?.clone()).ok()?
    else {
        return None;
    };
    let blocks = ::chat::resolve_assigned_mentions(blocks, &key_mentions).ok()?;
    let handle = ::chat::index::party_handle(&actor);
    let parties = me.map(|key| names.parties_of(key)).unwrap_or_default();
    Some(Arrival {
        room: room_name(&channel_id),
        in_my_dm: in_my_dm(&channel_id),
        channel_id,
        author: ::chat::client::author_display(&handle, names),
        body: ::chat::client::message_body_with_names(&blocks, names),
        mentions_me: ::chat::client::mentions_reach(&blocks, &parties),
        authored_by_me: me.is_some_and(|key| names.owns_handle(&handle, key)),
    })
}

// ============================================================================
// the platform call — the one part no test reaches
// ============================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::{DesktopNotice, HostNotifier, count_refusal, record_host};

    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };

    /// UNUserNotificationCenter TERMINATES a process with no bundle
    /// identifier — it is not an error a caller can catch, the process dies.
    /// `cargo test`, `cargo run` and any bare binary are exactly that process,
    /// so the identifier is read first; `boot` says so once and `post` skips.
    fn bundled() -> bool {
        // SAFETY: reading the main bundle's identifier is valid on any thread.
        unsafe { NSBundle::mainBundle().bundleIdentifier().is_some() }
    }

    /// An NSError as two loggable fields: its domain and code. `UNErrorDomain`
    /// code 1 is "notifications are not allowed for this application" — the
    /// person refused, or the prompt is still up.
    fn error_fields(error: *mut NSError) -> (String, isize) {
        if error.is_null() {
            return (String::new(), 0);
        }
        // SAFETY: the framework hands a live NSError for the handler's duration.
        let error = unsafe { &*error };
        (error.domain().to_string(), error.code())
    }

    /// ASK AT LAUNCH. macOS prompts the first time a bundle id asks and stores
    /// the answer; every later ask returns the stored answer without a prompt.
    /// The answer lands in one log line and in `HOST`.
    pub(super) fn boot() {
        if !bundled() {
            record_host(HostNotifier::Unbundled);
            tracing::info!(
                target: "ducktape::app",
                reason = "no_bundle_identifier",
                "desktop notifications are off: this process is not an app bundle — `make install` and launch Ducktape.app"
            );
            return;
        }
        let options = UNAuthorizationOptions::UNAuthorizationOptionAlert
            | UNAuthorizationOptions::UNAuthorizationOptionSound;
        let handler = block2::RcBlock::new(|granted: Bool, error: *mut NSError| {
            let (error_domain, error_code) = error_fields(error);
            if granted.as_bool() {
                record_host(HostNotifier::Ready);
                tracing::info!(
                    target: "ducktape::app",
                    reason = "authorized",
                    "desktop notifications are on"
                );
                return;
            }
            record_host(HostNotifier::Denied);
            tracing::warn!(
                target: "ducktape::app",
                reason = "authorization_denied",
                error_domain = %error_domain,
                error_code,
                "desktop notifications are off: allow Ducktape in System Settings → Notifications"
            );
        });
        // SAFETY: a plain framework call; the center is thread-safe by
        // contract and the block is copied by the framework.
        unsafe {
            UNUserNotificationCenter::currentNotificationCenter()
                .requestAuthorizationWithOptions_completionHandler(options, &handler);
        }
    }

    pub(super) fn post(notice: &DesktopNotice) {
        if !bundled() {
            tracing::debug!(
                target: "ducktape::app",
                reason = "no_bundle_identifier",
                room = %notice.thread,
                "skipped a desktop notification"
            );
            return;
        }
        let room = notice.thread.clone();
        // The host's word on THIS request: a refusal names why in the ring,
        // and an acceptance heals a stale `Denied` — a person who allowed the
        // app in System Settings after boot is not made to relaunch.
        let handler = block2::RcBlock::new(move |error: *mut NSError| {
            if error.is_null() {
                record_host(HostNotifier::Ready);
                return;
            }
            record_host(HostNotifier::Denied);
            let (error_domain, error_code) = error_fields(error);
            let attempts = count_refusal();
            let first_refusal = attempts == 1;
            if first_refusal {
                tracing::warn!(
                    target: "ducktape::app",
                    reason = "notify_refused",
                    error_domain = %error_domain,
                    error_code,
                    attempts,
                    room = %room,
                    "the host refused a desktop notification"
                );
                return;
            }
            tracing::debug!(
                target: "ducktape::app",
                reason = "notify_refused",
                error_domain = %error_domain,
                error_code,
                attempts,
                room = %room,
                "the host refused a desktop notification"
            );
        });
        // SAFETY: every call below is a plain framework call on objects this
        // function owns; the center is thread-safe by contract and copies the
        // block.
        unsafe {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&notice.title));
            content.setSubtitle(&NSString::from_str(&notice.subtitle));
            content.setBody(&NSString::from_str(&notice.body));
            content.setThreadIdentifier(&NSString::from_str(&notice.thread));
            content.setSound(Some(&UNNotificationSound::defaultSound()));
            let id = NSString::from_str(&format!("ducktape-{}", fresh_notice_id()));
            let request: Retained<UNNotificationRequest> =
                UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
            center.addNotificationRequest_withCompletionHandler(&request, Some(&*handler));
        }
    }

    /// A fresh identifier per banner — reusing one REPLACES the standing
    /// notification instead of adding to it.
    fn fresh_notice_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}

/// The freedesktop notifications service: one `Notify` call on the session
/// bus, which every desktop off macOS answers through its own notification
/// daemon. Pure Rust, and no new dependency — zbus is already in this binary
/// under the accessibility stack.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{DesktopNotice, HostNotifier, count_refusal, record_host};
    use std::collections::HashMap;
    use std::sync::OnceLock;

    /// The desktop entry the banner is filed under (`app/packaging`), which
    /// is what gives it this app's icon and lets the desktop group it.
    const DESKTOP_ENTRY: &str = "dev.ducktape.app";

    /// One session-bus connection per process, opened at boot. A host with no
    /// session bus answers every banner with the same skip, and says so once.
    fn session_bus() -> Option<&'static zbus::blocking::Connection> {
        static BUS: OnceLock<Option<zbus::blocking::Connection>> = OnceLock::new();
        BUS.get_or_init(|| match zbus::blocking::Connection::session() {
            Ok(connection) => {
                record_host(HostNotifier::Ready);
                Some(connection)
            }
            Err(error) => {
                record_host(HostNotifier::Unavailable);
                tracing::info!(
                    target: "ducktape::app",
                    reason = "no_session_bus",
                    %error,
                    "desktop notifications are off on this host"
                );
                None
            }
        })
        .as_ref()
    }

    /// The session bus is the whole authorization here: opening it is the
    /// ask, and the answer is logged where it is learned.
    pub(super) fn boot() {
        let _ = session_bus();
    }

    /// The freedesktop body is markup for the servers that render it, so the
    /// message's own angle brackets and ampersands are escaped rather than
    /// swallowed as tags.
    pub(super) fn markup_escaped(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub(super) fn post(notice: &DesktopNotice) {
        let Some(bus) = session_bus() else {
            tracing::debug!(
                target: "ducktape::app",
                reason = "no_session_bus",
                room = %notice.thread,
                "skipped a desktop notification"
            );
            return;
        };
        let bus = bus.clone();
        let notice = notice.clone();
        // The live fold must not wait on the bus: the call runs on its own
        // thread and reports there.
        std::thread::spawn(move || {
            let Err(error) = notify(&bus, &notice) else {
                return;
            };
            let attempts = count_refusal();
            let first_refusal = attempts == 1;
            if first_refusal {
                tracing::warn!(
                    target: "ducktape::app",
                    reason = "notify_refused",
                    attempts,
                    room = %notice.thread,
                    %error,
                    "the notification daemon refused a desktop notification"
                );
                return;
            }
            tracing::debug!(
                target: "ducktape::app",
                reason = "notify_refused",
                attempts,
                room = %notice.thread,
                %error,
                "the notification daemon refused a desktop notification"
            );
        });
    }

    /// One `Notify` call: `(app_name, replaces_id, app_icon, summary, body,
    /// actions, hints, expire_timeout)` → the banner's id. A fresh id per
    /// banner, the desktop's own timeout, and no actions — the bell is where
    /// a reader acts.
    pub(super) fn notify(
        bus: &zbus::blocking::Connection,
        notice: &DesktopNotice,
    ) -> zbus::Result<u32> {
        let body = markup_escaped(&format!("{}: {}", notice.subtitle, notice.body));
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([
            ("desktop-entry", zbus::zvariant::Value::from(DESKTOP_ENTRY)),
            ("category", zbus::zvariant::Value::from("im.received")),
        ]);
        let reply = bus.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Ducktape",
                0u32,
                "",
                notice.title.as_str(),
                body.as_str(),
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )?;
        reply.body().deserialize::<u32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fold's "is anyone looking" is what the windows last reported, on
    /// every host: focus taken is looking, focus lost is elsewhere.
    #[test]
    fn the_focus_fact_is_what_the_windows_last_reported() {
        let _ = note_window_focus(true);
        assert!(app_focused());
        let _ = note_window_focus(false);
        assert!(!app_focused());
    }

    /// The host's answer crosses to the Settings view as a stable token, one
    /// per variant, and round-trips through the byte it is stored as.
    #[test]
    fn every_host_answer_has_its_own_token_and_survives_storage() {
        let answers = [
            HostNotifier::Pending,
            HostNotifier::Ready,
            HostNotifier::Unbundled,
            HostNotifier::Denied,
            HostNotifier::Unavailable,
        ];
        let tokens: BTreeSet<&str> = answers.iter().map(|answer| answer.token()).collect();
        assert_eq!(tokens.len(), answers.len(), "two answers share a token");
        for answer in answers {
            assert_eq!(HostNotifier::from_u8(answer as u8), answer);
        }
    }

    /// A message's own markup characters reach the freedesktop body escaped,
    /// so a notification server that renders markup shows them as typed.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_freedesktop_body_escapes_the_message_markup() {
        assert_eq!(platform::markup_escaped("a <b> & c"), "a &lt;b&gt; &amp; c");
    }

    /// THE DRIVE: a banner through this session's own notification daemon.
    /// Ignored because it needs a session bus with a daemon on it and shows a
    /// real banner; run it by name on a desktop to see the arm work.
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "needs a session bus with a notification daemon; shows a banner"]
    fn the_freedesktop_arm_posts_a_banner_the_daemon_accepts() {
        let bus = zbus::blocking::Connection::session().expect("a session bus");
        let id = platform::notify(
            &bus,
            &DesktopNotice {
                title: "#general".into(),
                subtitle: "Reader".into(),
                body: "a <drive> banner & nothing more".into(),
                thread: "drive".into(),
            },
        )
        .expect("the daemon takes the banner");
        assert!(id > 0, "a banner has a nonzero id, got {id}");
    }

    #[test]
    fn notification_uses_canonical_program_author_and_account_mentions() {
        let key = [7; 32];
        let names = NameDirectory::from_accounts(&[
            identity::AccountView {
                number: 1,
                name: "Reader".into(),
                control: identity::Control::Keys,
                keys: vec![identity::KeyView {
                    scheme: identity::KeyScheme::Ed25519,
                    pubkey: key.to_vec(),
                    label: None,
                    added_at: 0,
                }],
                avatar: None,
                bio: None,
                updated_at: 0,
            },
            identity::AccountView {
                number: 2,
                name: "Reporter".into(),
                control: identity::Control::Program {
                    controller: 1,
                    executor: "agent".into(),
                    generation: 0,
                    standing: identity::ProgramStanding::Active,
                },
                keys: Vec::new(),
                avatar: None,
                bio: None,
                updated_at: 0,
            },
        ]);
        let payload = ::chat::encode_msg(&::chat::ChatMsg::PostMessage {
            channel_id: "notifications-test".into(),
            message_id: "m".into(),
            blocks: vec![::chat::Block::Paragraph(vec![::chat::Span {
                text: "@Old Name".into(),
                marks: vec![::chat::Mark::Mention(::chat::Party::Key(key.to_vec()))],
            }])],
            thread: None,
        });
        let assigned = serde_json::to_value(::chat::ChatAssigned::Posted {
            seq: 1,
            actor: ::chat::Party::Account(2),
            key_mentions: vec![1],
        })
        .unwrap();
        let arrival = chat_arrival(&payload, Some(&assigned), &names, Some(&key)).unwrap();
        assert_eq!(arrival.author, "Reporter");
        assert_eq!(arrival.body, "@Reader");
        assert!(arrival.mentions_me);
        assert!(!arrival.authored_by_me);
        assert!(chat_arrival(&payload, None, &names, Some(&key)).is_none());
        let mine = serde_json::to_value(::chat::ChatAssigned::Posted {
            seq: 2,
            actor: ::chat::Party::Account(1),
            key_mentions: vec![1],
        })
        .unwrap();
        let own = chat_arrival(&payload, Some(&mine), &names, Some(&key)).unwrap();
        assert!(own.authored_by_me);
        assert!(
            notice_reason(&own).is_none(),
            "the source account is authoritative across devices"
        );
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
