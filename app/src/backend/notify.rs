//! Platform notifications and cached inputs for the deployed Chat view.
//! Chat owns notification selection and wording; the host records device
//! preferences/focus and submits the resulting text to the OS notifier.

use super::*;

#[cfg(test)]
use std::collections::BTreeSet;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// One notification, already worded — the shape the platform call takes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct DesktopNotice {
    /// the room, as the sidebar says it: `#general`, or the peer's name.
    pub title: String,
    /// who wrote it, by account name.
    pub subtitle: String,
    pub body: String,
    /// the room, for grouping every notice from one conversation together.
    pub thread: String,
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

/// The visible room, recorded by native navigation for notification context.
static ACTIVE_CHANNEL: RwLock<String> = RwLock::new(String::new());
/// Record the currently visible room for the view's notification policy.
pub(crate) fn note_active_channel(active: &str) {
    if let Ok(mut open) = ACTIVE_CHANNEL.write() {
        active.clone_into(&mut open);
    }
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

/// Forward committed operations and cached host facts. The deployed view
/// recognizes arrivals, resolves mention assignments, and decides the banner.
pub(crate) fn notify_chat_op(rpc: &str, payload: &[u8], assigned: Option<&serde_json::Value>) {
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    notify_from_view(
        rpc,
        serde_json::json!({
            "payload":payload, "assigned":assigned,
            "context":{
                "key":rpc::cached_user_key().unwrap_or_default(),
                "screen":{"app_focused":app_focused(), "active_channel":active_channel()}
            }
        }),
    );
}

/// Start before queuing the consumer, so a reconnect cannot move an arrival
/// onto another network. The live fold never waits for view execution.
fn notify_from_view(rpc: &str, request: serde_json::Value) {
    if !notifications_enabled() {
        return;
    }
    let props = serde_json::to_vec(&serde_json::json!({
        "background":{"kind":"notice", "request":request}
    }))
    .expect("notification inputs encode");
    let Ok(mut session) = crate::module_view::background::start("chat", props, rpc) else {
        return;
    };
    let signer = rpc::cached_user_key();
    tokio::spawn(async move {
        use futures::StreamExt as _;
        let mut output = Vec::new();
        while let Some(chunk) = session.events.next().await {
            let Ok(chunk) = chunk else {
                return;
            };
            output.extend_from_slice(&chunk);
        }
        let current = session.is_current() && signer == rpc::cached_user_key();
        if !current || !notifications_enabled() {
            return;
        }
        let Ok(result) = serde_json::from_slice::<serde_json::Value>(&output) else {
            return;
        };
        let Ok(notice) = serde_json::from_value::<DesktopNotice>(result["notice"].clone()) else {
            return;
        };
        platform::post(&notice);
    });
}

/// How many posts the host has refused this boot. The first refusal is a
/// `warn!`; the rest are `debug!` carrying this count, so a standing refusal
/// is one line in the ring and not a line per mention.
fn count_refusal() -> u64 {
    use std::sync::atomic::AtomicU64;
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    REFUSED.fetch_add(1, Ordering::Relaxed) + 1
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
}
