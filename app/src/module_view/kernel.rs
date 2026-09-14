//! The kernel contract: what EVERY module view may ask of the app, with no
//! per-module code on this side of the wire. A view that speaks only this
//! contract is replaced by a module deployment alone — the app binary
//! never changes for it.
//!
//! - `rpc.query` `{target, query}` — one module query on the connected
//!   node, answered with the reply's JSON; `rpc.view` the same against
//!   the module's index-tier view.
//! - `rpc.blocks` `{limit}` — the recent block feed; `rpc.status` and
//!   `rpc.peers` the node's own status and peers JSON.
//! - `rpc.live` `<plane>` — a subscription that gets one item per block
//!   the app's live stream reports for that plane: a module's name
//!   ([`live_hit`], any module — an audited view is trusted to read what
//!   it names), or `block` for every block ([`block_hit`]), so the view
//!   re-reads what moved.
//! - `rpc.stream` `{topic, params}` — a subscription on ONE of the node's
//!   own event topics, opened over `/v1/ws` with `params` as the upgrade's
//!   query and the SEATED key as the proof on it. Every frame the node
//!   sends arrives as one item, byte for byte; the close is the item that
//!   ends it. The kernel reads neither the topic nor the frames — the node
//!   decides what this key may hear.
//! - `rpc.query_as_reader` `{target, query}` — the query AS THE SEATED
//!   KEY'S HOLDER (`/v1/query/reader`), for a module that serves protected
//!   content to its reader; refused while the key is locked.
//! - `files.get` `{lane, params}` — one `/v1/files/<lane>` read with its
//!   query-string params; `blob.get` `{digest, limit}` — a blob by hex
//!   digest, verified against it.
//! - `picture.load` `{surface, path}` — a duckfs file paged in, decoded and
//!   parked in a host picture surface's slot; answered with its drawn size.
//! - `files.stage` `<raw bytes>` / `blob.put` `<raw bytes>` — a duckfs
//!   chunk or a blob landed on the node, proven with the seated key;
//!   answered with the digest.
//! - `op.submit` `{target, payload}` — one module op, signed with the
//!   SEATED key and submitted; answered with the block height. The view
//!   never carries a password, an endpoint or a key.
//! - `rpc.admin` `{route, payload}` — one POST to a `/v1` route that
//!   mutates THE NODE rather than module state, signed with the SEATED key
//!   exactly as the `ducktape node` verbs sign theirs; answered with the
//!   node's own reply text, or its refusal. The kernel names no route —
//!   the node's operator gate decides what this key may ask for.
//! - `git.merge` `{target, repo, ours, theirs, message}` — the merge commit
//!   a git-backed module's wire demands the CLIENT compute, built with
//!   libgit2 against a bare mirror of the node's `/<target>/<repo>` remote
//!   and landed as a pack under the seated key; answered `{merge_oid,
//!   pack_digest}`, or `{conflicts}` and nothing written. No wasm guest can
//!   do this: it is a git implementation plus a second transport.
//! - `picture.put` `{surface, path, pages}` — base64 pages decoded, joined
//!   and parked as the picture the `picture` surface draws under `surface`;
//!   answered `{width, height}`. `picture.inline` `{doc, source, base, net}`
//!   — the pictures a Markdown `source` embeds, resolved against the
//!   document's own `duck://` address and parked under `doc` for the
//!   document surface. Both are the app's decoder and its one outbound
//!   picture gate, which a view has neither of.
//! - `host.badge` `<count>` — the tab badge, handed to the app as the
//!   `badge` event with `{"count": N}` in its detail; `host.roster`
//!   `{scope, members}` — the mention vocabulary of the host composer a
//!   view docked over `scope`.
//! - `host.id` `<prefix>` — one id, unique on this device, for a module
//!   whose records are addressed by ids its WRITER mints. A view has no
//!   clock and no entropy of its own, so the app mints it.
//! - `clock.ticks` `<period, i64 ms little-endian>` — a subscription that
//!   gets one item per period. A wasm module has no clock, so the guest's
//!   recurring tasks use this door; the window thread keeps the
//!   deadline and the shell draws the frame it comes due on.
//!
//! A query and a submit go to the node off the window thread, on the
//! kernel's own runtime, and their answers wait in [`Replies`] for the
//! view's next redraw; reply notifications wake the native presenter.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{Guest, ModuleViewEvent, Slot, wire};

/// The most blocks one `rpc.blocks` may ask for.
const MAX_BLOCKS: usize = 1_000;
/// The most a `blob.get` may pull: a frame's worth, as the loader's own cap.
const MAX_BLOB_BYTES: usize = 16 << 20;
/// The longest `host.id` prefix: a word naming the kind of record, not a
/// payload of its own.
const MAX_ID_PREFIX: usize = 32;
/// The most one `rpc.stream` frame may carry into a view. A node frame is a
/// line of a run's output or the like; a frame past this is the node
/// misbehaving, and the subscription ends rather than growing the guest.
const MAX_STREAM_FRAME_BYTES: usize = 1 << 20;
const MAX_IN_FLIGHT: usize = 256;
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_REPLY_EVENTS: usize = 1024;
const MAX_REPLY_BYTES: usize = 32 << 20;

/// The kernel's answers to a view's requests, written off-thread and
/// drained into the guest's pending events at its next redraw.
pub(super) struct Replies {
    events: Mutex<Vec<wire::Event>>,
    in_flight: AtomicUsize,
    /// Told on every answer delivered: a test waits here for the node
    /// calls in flight, never on a clock.
    landed: std::sync::Condvar,
    changed: tokio::sync::watch::Sender<()>,
    fault: Mutex<Option<String>>,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            events: Mutex::default(),
            in_flight: AtomicUsize::new(0),
            landed: std::sync::Condvar::new(),
            changed: tokio::sync::watch::channel(()).0,
            fault: Mutex::default(),
        }
    }
}

impl Replies {
    /// Coalesced notifications wake each native presenter independently. The
    /// answer remains in the queue, including when no window is presenting it.
    pub(super) fn changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(super) fn drain_into(&self, pending: &mut Vec<wire::Event>) -> Result<(), String> {
        let mut events = self.events.lock().expect("kernel replies");
        if let Some(fault) = self.fault() { return Err(fault); }
        pending.append(&mut events);
        Ok(())
    }

    pub(super) fn fault(&self) -> Option<String> {
        self.fault.lock().expect("kernel reply fault").clone()
    }

    fn admit(self: &std::sync::Arc<Self>) -> Option<InFlight> {
        if self.fault().is_some() { return None; }
        self.in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
            |count| (count < MAX_IN_FLIGHT).then_some(count + 1)).ok()?;
        Some(InFlight(self.clone()))
    }

    /// Whether a query or a submit is still on its way.
    #[cfg(test)]
    pub(super) fn any_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    /// WHETHER THE VIEW IS STILL OWED A FRAME, which is the in-flight count
    /// AND the answers already lying here. The two are one fact to a caller
    /// and reading only the count loses a race it loses often: a request is
    /// spawned inside a redraw, and a node that answers before that redraw
    /// returns has already given the count back — leaving an answer nobody
    /// is coming back for. The widget then stops polling and the view sits
    /// on "Loading…" until an unrelated event wakes it; a test's pump
    /// returns and reads a screen that never got its rows.
    ///
    /// Under the events lock, because that is the lock [`Replies::settled`]
    /// takes to give a count back: with it held, empty and zero together
    /// mean nothing can arrive that no one is waiting for.
    pub(super) fn answer_owed(&self) -> bool {
        let events = self.events.lock().expect("kernel replies");
        !events.is_empty() || self.in_flight.load(Ordering::SeqCst) > 0
    }

    /// Blocks until nothing is in flight.
    #[cfg(test)]
    pub(super) fn wait_idle(&self) {
        let mut events = self.events.lock().expect("kernel replies");
        while self.any_in_flight() {
            events = self.landed.wait(events).expect("kernel replies");
        }
    }

    /// One item for a request the kernel is running; `done` ends it for the
    /// guest. The in-flight count is [`Replies::settled`]'s to give back —
    /// a subscription's last item and its count are not the same moment.
    fn item(&self, id: u64, result: Result<Vec<u8>, String>, done: bool) {
        let mut events = self.events.lock().expect("kernel replies");
        if self.fault().is_some() { return; }
        let bytes = |result: &Result<Vec<u8>, String>| match result {
            Ok(bytes) => bytes.len(), Err(error) => error.len(),
        };
        let queued: usize = events.iter().map(|event| match event {
            wire::Event::Response { result, .. } => bytes(result),
            _ => 0,
        }).sum();
        let exceeds_budget = events.len() >= MAX_REPLY_EVENTS
            || bytes(&result) > MAX_REPLY_BYTES.saturating_sub(queued);
        if exceeds_budget {
            *self.fault.lock().expect("kernel reply fault") = Some("view reply backlog limit exceeded; view stopped".into());
            self.landed.notify_all();
            self.changed.send_replace(());
            return;
        }
        events.push(wire::Event::Response { id, result, done });
        self.landed.notify_all();
        self.changed.send_replace(());
    }

    /// One request off the in-flight count, under the lock a waiter holds.
    fn settled(&self) {
        let _events = self.events.lock().expect("kernel replies");
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.landed.notify_all();
        self.changed.send_replace(());
    }

    #[cfg(test)]
    fn deliver(&self, id: u64, result: Result<Vec<u8>, String>) {
        self.item(id, result, true);
        self.settled();
    }
}

/// The in-flight count one subscription took, given back when its task
/// ends — INCLUDING THE ABORT a cancel or a replaced view fires, which is
/// the only way a socket waiting on the node stops waiting. Without this
/// the count would outlive the socket and the widget would poll forever.
struct InFlight(std::sync::Arc<Replies>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.settled();
    }
}

#[cfg(test)]
#[test]
fn reply_notifications_wake_each_presenter_and_keep_the_answer() {
    let replies = Replies::default();
    let mut first = replies.changes();
    let mut second = replies.changes();
    replies.item(7, Ok(vec![1, 2]), true);
    futures::executor::block_on(async {
        first.changed().await.expect("first presenter notified");
        second.changed().await.expect("second presenter notified");
    });
    let mut pending = Vec::new();
    replies.drain_into(&mut pending).expect("reply budget");
    assert!(matches!(pending.as_slice(), [wire::Event::Response { id: 7, result: Ok(bytes), done: true }] if bytes == &[1, 2]));
    assert!(!replies.answer_owed());
}

#[cfg(test)]
#[test]
fn request_admission_is_bounded_and_drop_returns_capacity() {
    let replies = std::sync::Arc::new(Replies::default());
    let mut admitted: Vec<_> = (0..MAX_IN_FLIGHT)
        .map(|_| replies.admit().expect("within budget")).collect();
    assert!(replies.admit().is_none());
    admitted.pop();
    let replacement = replies.admit().expect("dropped request returns capacity");
    drop(replacement);
    drop(admitted);
    assert!(!replies.any_in_flight());
}

#[cfg(test)]
#[test]
fn reply_overflow_stops_the_view_instead_of_losing_an_answer_silently() {
    let replies = std::sync::Arc::new(Replies::default());
    for id in 0..MAX_REPLY_EVENTS { replies.item(id as u64, Ok(Vec::new()), false); }
    assert!(replies.fault().is_none());
    replies.item(MAX_REPLY_EVENTS as u64, Ok(Vec::new()), true);
    assert!(replies.fault().is_some());
    assert!(replies.admit().is_none());
    let mut pending = Vec::new();
    assert!(replies.drain_into(&mut pending).is_err());
    assert!(pending.is_empty());
    assert_eq!(replies.events.lock().unwrap().len(), MAX_REPLY_EVENTS);
}

#[cfg(test)]
#[test]
fn queued_reply_bytes_are_bounded_across_individually_valid_items() {
    let replies = Replies::default();
    replies.item(1, Ok(vec![0; MAX_REPLY_BYTES]), false);
    assert!(replies.fault().is_none());
    replies.item(2, Ok(vec![1]), true);
    assert!(replies.fault().is_some());
    assert_eq!(replies.events.lock().unwrap().len(), 1);
}

/// The kernel's own runtime, on its own thread: the window thread never
/// blocks on the node, and the app's executor is not this module's to use.
pub(super) fn runtime() -> tokio::runtime::Handle {
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let (send, recv) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("views-kernel".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("the views kernel runtime");
                    send.send(runtime.handle().clone())
                        .expect("the kernel handle is taken");
                    runtime.block_on(std::future::pending::<()>());
                })
                .expect("the views kernel thread");
            recv.recv().expect("the kernel runtime came up")
        })
        .clone()
}

/// Routes one kernel request; `false` when the kind is not the kernel's.
pub(super) fn answer(
    guest: &mut Guest,
    capability: &str,
    operation: &str,
    id: u64,
    payload: &[u8],
) -> bool {
    match (capability, operation) {
        ("rpc", "query") => spawn(guest, id, payload, query),
        ("rpc", "view") => spawn(guest, id, payload, view),
        ("rpc", "blocks") => spawn(guest, id, payload, blocks),
        ("rpc", "status") => spawn(guest, id, b"{}", status),
        ("rpc", "peers") => spawn(guest, id, b"{}", peers),
        ("rpc", "query_as_reader") => spawn(guest, id, payload, query_as_reader),
        ("rpc", "stream") => stream_open(guest, id, payload),
        ("files", "get") => spawn(guest, id, payload, files_get),
        ("files", "stage") => spawn_raw(guest, id, payload, files_stage),
        ("picture", "load") => spawn(guest, id, payload, picture_load),
        ("blob", "get") => spawn(guest, id, payload, blob_get),
        ("blob", "put") => spawn_raw(guest, id, payload, blob_put),
        ("rpc", "live") => {
            let plane = std::str::from_utf8(payload).unwrap_or_default().trim();
            let named = plane == BLOCK_PLANE || workspace_config::validate_module_id(plane).is_ok();
            let capacity = guest.live_subscriptions.len() < MAX_SUBSCRIPTIONS;
            if !capacity { guest.refuse(id, "too many live subscriptions".into()); return true; }
            match named {
                true => guest.live_subscriptions.push((id, plane.to_owned())),
                false => guest.refuse(id, "`rpc.live` names no plane".into()),
            }
        }
        ("op", "submit") => spawn(guest, id, payload, submit),
        ("rpc", "admin") => spawn(guest, id, payload, admin),
        ("git", "merge") => spawn(guest, id, payload, git_merge),
        ("picture", "put") => spawn_host(guest, id, payload, picture_put),
        ("picture", "inline") => spawn(guest, id, payload, picture_inline),
        ("host", "roster") => match host_roster(payload) {
            Some((scope, members)) => {
                crate::composer_surface::roster(&scope, &members);
                guest.reply(id, Ok(Vec::new()));
            }
            None => guest.refuse(id, "`host.roster` names no scope".into()),
        },
        ("host", "badge") => {
            let count = std::str::from_utf8(payload)
                .ok()
                .and_then(|text| text.trim().parse::<i64>().ok());
            match count {
                Some(count) => {
                    guest.intents.push(ModuleViewEvent {
                        kind: "badge".into(),
                        detail: format!("{{\"count\":{count}}}"),
                    });
                    guest.reply(id, Ok(Vec::new()));
                }
                None => guest.refuse(id, "`host.badge` carries no count".into()),
            }
        }
        ("clock", "ticks") => {
            let period = tick_period(payload);
            let capacity = guest.clocks.len() < MAX_SUBSCRIPTIONS;
            if !capacity { guest.refuse(id, "too many clock subscriptions".into()); return true; }
            match period {
                Some(period) => guest.clocks.push(Clock {
                    id,
                    period,
                    due: std::time::Instant::now() + period,
                }),
                None => guest.refuse(id, "`clock.ticks` names no period".into()),
            }
        }
        ("host", "id") => {
            let prefix = std::str::from_utf8(payload).unwrap_or_default().trim();
            let named = !prefix.is_empty()
                && prefix.len() <= MAX_ID_PREFIX
                && prefix.bytes().all(|byte| byte.is_ascii_alphanumeric());
            match named {
                true => guest.reply(
                    id,
                    Ok(crate::backend::fresh_id(prefix).into_bytes()),
                ),
                false => guest.refuse(id, "`host.id` names no prefix".into()),
            }
        }
        _ => return false,
    }
    true
}

type Call = fn(
    ducktape_rpc::Client,
    serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>;

/// Runs one node call for a request: decoded here, answered on the kernel
/// runtime, delivered at the guest's next redraw.
fn spawn(guest: &mut Guest, id: u64, payload: &[u8], call: Call) {
    let ask: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(ask) => ask,
        Err(error) => {
            guest.refuse(id, format!("request is not JSON: {error}"));
            return;
        }
    };
    let client = super::connection()
        .lock()
        .expect("views rpc")
        .client
        .clone();
    let Some(client) = client else {
        guest.refuse(id, "not connected to a node".into());
        return;
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    runtime().spawn(async move {
        let _counted = counted;
        let result = call(client, ask).await;
        replies.item(id, result, true);
    });
}

type HostCall = fn(
    serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>;

/// [`spawn`] for a door the NODE is not part of — the app's own picture
/// store. It takes no client, so it answers whether or not the app has a
/// connection: refusing one of these while offline would be a lie about
/// where the work happens.
fn spawn_host(guest: &mut Guest, id: u64, payload: &[u8], call: HostCall) {
    let ask: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(ask) => ask,
        Err(error) => {
            guest.refuse(id, format!("request is not JSON: {error}"));
            return;
        }
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    runtime().spawn(async move {
        let _counted = counted;
        let result = call(ask).await;
        replies.item(id, result, true);
    });
}

type RawCall = fn(
    ducktape_rpc::Client,
    Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>;

/// [`spawn`] for a request whose payload is the bytes themselves.
fn spawn_raw(guest: &mut Guest, id: u64, payload: &[u8], call: RawCall) {
    let client = super::connection()
        .lock()
        .expect("views rpc")
        .client
        .clone();
    let Some(client) = client else {
        guest.refuse(id, "not connected to a node".into());
        return;
    };
    let bytes = payload.to_vec();
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    runtime().spawn(async move {
        let _counted = counted;
        let result = call(client, bytes).await;
        replies.item(id, result, true);
    });
}

/// A node stream the kernel is running for one subscription. Dropped with
/// the guest that asked, or with the cancel that retires it — and dropping
/// it ends the socket, so a view that is replaced leaves nothing reading.
/// One `clock.ticks` subscription: the period the view asked for, and when
/// its next item is due.
pub(super) struct Clock {
    pub(super) id: u64,
    period: std::time::Duration,
    due: std::time::Instant,
}

/// The shortest and longest period a view may ask the clock for. Below the
/// floor a tick is a spin the window thread pays for every frame; above the
/// ceiling it is not a period but a date, which a view has no business
/// keeping — it reads the node for that.
const MIN_TICK_MS: i64 = 16;
const MAX_TICK_MS: i64 = 60 * 60 * 1_000;

/// A `clock.ticks` payload: the period in milliseconds, little-endian, as
/// `ui_lang_guest::every` writes it.
fn tick_period(payload: &[u8]) -> Option<std::time::Duration> {
    let millis = i64::from_le_bytes(<[u8; 8]>::try_from(payload).ok()?);
    let named = (MIN_TICK_MS..=MAX_TICK_MS).contains(&millis);
    named.then(|| std::time::Duration::from_millis(millis as u64))
}

/// Every clock item due at `now`, and the deadline re-armed for each. The
/// instant is an argument so the rule is decided, not timed: the widget
/// hands it `Instant::now()`, a test hands it the deadline it chose.
pub(super) fn ticked(clocks: &mut [Clock], now: std::time::Instant) -> Vec<wire::Event> {
    let mut items = Vec::new();
    for clock in clocks.iter_mut() {
        if clock.due > now {
            continue;
        }
        // ONE ITEM PER REDRAW, however far behind: a window that was not
        // drawn for a minute owes the view one tick, not four thousand.
        clock.due = now + clock.period;
        items.push(wire::Event::Response {
            id: clock.id,
            result: Ok(Vec::new()),
            done: false,
        });
    }
    items
}

/// When the nearest clock item comes due, for the redraw the widget asks
/// the shell to schedule.
pub(super) fn next_tick(clocks: &[Clock]) -> Option<std::time::Instant> {
    clocks.iter().map(|clock| clock.due).min()
}

pub(super) struct NodeStream(tokio::task::JoinHandle<()>);

impl Drop for NodeStream {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `{topic, params}` read once: the topic to ask the node for, and the
/// `/v1/ws` query the signature will cover. A param that is not a plain
/// token is REFUSED rather than escaped — the signed string and the
/// requested string must be the same one, and a view has nothing to name
/// here that is not a token.
fn stream_ask(ask: &serde_json::Value) -> Result<(String, String), String> {
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:~".contains(&byte))
    };
    let topic = ask["topic"].as_str().unwrap_or_default();
    if !plain(topic) {
        return Err("`rpc.stream` names no topic".into());
    }
    let mut query = String::new();
    for (key, value) in ask["params"].as_object().into_iter().flatten() {
        let value = value.as_str().unwrap_or_default();
        if !plain(key) || !plain(value) {
            return Err("`rpc.stream` takes plain token params".into());
        }
        let separator = match query.is_empty() {
            true => '?',
            false => '&',
        };
        query.push(separator);
        query.push_str(key);
        query.push('=');
        query.push_str(value);
    }
    Ok((topic.to_owned(), query))
}

/// Opens one node topic for a view: the socket under the seated key, then
/// every frame it sends, until it ends.
fn stream_open(guest: &mut Guest, id: u64, payload: &[u8]) {
    guest.streams.retain(|(_, stream)| !stream.0.is_finished());
    let ask: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(ask) => ask,
        Err(error) => {
            guest.refuse(id, format!("request is not JSON: {error}"));
            return;
        }
    };
    let (topic, query) = match stream_ask(&ask) {
        Ok(named) => named,
        Err(refusal) => {
            guest.refuse(id, refusal);
            return;
        }
    };
    let client = super::connection()
        .lock()
        .expect("views rpc")
        .client
        .clone();
    let Some(client) = client else {
        guest.refuse(id, "not connected to a node".into());
        return;
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let handle = runtime().spawn(async move {
        let _counted = counted;
        match open_topic(client.origin(), &topic, &query).await {
            Ok(socket) => forward(&replies, id, socket).await,
            Err(error) => replies.item(id, Err(error), true),
        }
    });
    guest.streams.push((id, NodeStream(handle)));
}

/// The node's own event socket for ONE topic, proven with the SEATED key:
/// the signature rides the upgrade, over the same path the request carries,
/// and the node admits or refuses this key for that topic before the socket
/// exists. Nothing here is per-topic — the app's run-output watcher presents
/// exactly this proof, and this is that door with the run taken out of it.
async fn open_topic(
    rpc: &str,
    topic: &str,
    query: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let node_key = crate::backend::node_public_key(rpc).await?;
    let signed =
        crate::backend::seated_request_headers("GET", &format!("/v1/ws{query}"), &node_key, b"")
            .await
            .ok_or_else(|| "`rpc.stream` needs the session key unlocked".to_owned())?;
    let mut request = format!("{}{query}", crate::backend::agent_ws_url(rpc))
        .into_client_request()
        .map_err(|error| format!("could not address the node: {error}"))?;
    for (name, value) in signed {
        let value = HeaderValue::from_str(&value)
            .map_err(|error| format!("the signature is not a header value: {error}"))?;
        request
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("could not open the node stream: {error}"))?;
    let subscribe = serde_json::json!({"op": "subscribe", "topics": [topic]});
    socket
        .send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|error| format!("could not subscribe to the node stream: {error}"))?;
    Ok(socket)
}

/// Every frame an open node socket sends, as one item each, verbatim: the
/// kernel never reads a frame, so a topic it has never heard of needs no
/// code here. The node ending the socket is the `done` that ends the
/// subscription.
async fn forward<S>(replies: &Replies, id: u64, mut socket: S)
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message;
    while let Some(message) = socket.next().await {
        let frame = match message {
            Ok(Message::Text(text)) => text.into_bytes(),
            Ok(Message::Binary(bytes)) => bytes,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(error) => {
                replies.item(id, Err(format!("the node stream failed: {error}")), true);
                return;
            }
        };
        if frame.len() > MAX_STREAM_FRAME_BYTES {
            replies.item(
                id,
                Err(format!(
                    "a node stream frame carries more than {MAX_STREAM_FRAME_BYTES} bytes"
                )),
                true,
            );
            return;
        }
        replies.item(id, Ok(frame), false);
        if replies.fault().is_some() { return; }
    }
    replies.item(id, Ok(Vec::new()), true);
}

fn query_as_reader(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let signer = crate::backend::seated_data_plane_signer(&client).await?;
        let reply: serde_json::Value = client
            .with_write_auth(signer)
            .query_as_reader(&target, &ask["query"])
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn files_get(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let lane = ask["lane"].as_str().unwrap_or_default();
        let lane_named = !lane.is_empty()
            && lane
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !lane_named {
            return Err("`files.get` names no lane".into());
        }
        let params: Vec<(String, String)> = ask["params"]
            .as_object()
            .map(|params| {
                params
                    .iter()
                    .map(|(key, value)| {
                        let value = match value {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        (key.clone(), value)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let reply = client
            .files_get(lane, &params)
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

/// `picture.load` `{surface, path}` — the whole duckfs file paged in, decoded
/// off the runtime and parked in a host picture surface's slot, answered with
/// the size it will be drawn at. The decode and the widget are the host's (a
/// tree wire carries no pixels), so a view that wants one names the slot it
/// left and the file to put in it. The two slots are the ones
/// [`crate::backend::picture`] draws; anything else is refused rather than
/// growing the store.
/// The picture slot a request names, or `None` for anything that is not one
/// of the two [`crate::backend::picture`] draws — the store never grows a
/// slot nothing paints.
fn picture_surface(ask: &serde_json::Value) -> Option<&'static str> {
    use crate::backend::{CHAT_SURFACE, FILES_SURFACE, FORGE_SURFACE};
    match ask["surface"].as_str()? {
        FILES_SURFACE => Some(FILES_SURFACE),
        FORGE_SURFACE => Some(FORGE_SURFACE),
        CHAT_SURFACE => Some(CHAT_SURFACE),
        _ => None,
    }
}

fn picture_load(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    use crate::backend::{MAX_PICTURE_BYTES, store_picture};
    Box::pin(async move {
        let surface = picture_surface(&ask).ok_or("`picture.load` names no surface")?;
        let path = ask["path"].as_str().unwrap_or_default().to_owned();
        let Some(bytes) = crate::backend::files_read_all(&client, &path).await? else {
            return Err(format!(
                "picture larger than the {} MiB preview limit",
                MAX_PICTURE_BYTES >> 20
            ));
        };
        let size = bytes.len();
        let (width, height) = store_picture(surface, path, bytes)
            .await
            .map_err(|reason| format!("{size} binary bytes · did not decode: {reason}"))?;
        serde_json::to_vec(&serde_json::json!({ "width": width, "height": height }))
            .map_err(|error| error.to_string())
    })
}

fn files_stage(
    client: ducktape_rpc::Client,
    bytes: Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let signer = crate::backend::seated_data_plane_signer(&client).await?;
        let digest = client
            .with_write_auth(signer)
            .files_stage(bytes)
            .await
            .map_err(|error| error.to_string())?;
        Ok(digest.into_bytes())
    })
}

fn blob_put(
    client: ducktape_rpc::Client,
    bytes: Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let signer = crate::backend::seated_data_plane_signer(&client).await?;
        let digest = client
            .with_write_auth(signer)
            .put_blob(bytes)
            .await
            .map_err(|error| error.to_string())?;
        Ok(digest.into_bytes())
    })
}

fn blob_get(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let digest = crate::backend::hex_decode(ask["digest"].as_str().unwrap_or_default())?;
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| "`blob.get` digest is not 32 bytes".to_owned())?;
        let limit = ask["limit"].as_u64().unwrap_or(0) as usize;
        client
            .get_blob(&digest, limit.clamp(1, MAX_BLOB_BYTES))
            .await
            .map_err(|error| error.to_string())
    })
}

fn target_of(ask: &serde_json::Value) -> Result<String, String> {
    let target = ask["target"].as_str().unwrap_or_default();
    workspace_config::validate_module_id(target)
        .map_err(|error| format!("request names no module target: {error}"))?;
    Ok(target.to_owned())
}

fn query(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let reply: serde_json::Value = client
            .query(&target, &ask["query"])
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

/// One index-tier view read, AFTER the module's fold has caught up with
/// everything this client knows it wrote.
///
/// A derived read model folds BEHIND the block loop, so a view read fired on
/// the heels of this view's own `op.submit` answers a tier that predates it:
/// the moved block back where it was, the deleted line still alive, the line
/// just typed missing. A module whose records the view then plans against
/// (the pages document save) turns that into a DUPLICATE write, so the wait
/// belongs on the kernel's read rather than in each view that has to
/// remember it. `crate::backend::await_seen_fold` waits for nothing when
/// nothing is outstanding, which is every read a view makes that did not
/// just write.
fn view(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        crate::backend::await_seen_fold(&client, &target, &ask["query"]).await;
        let reply: serde_json::Value = client
            .view(&target, &ask["query"])
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn status(
    client: ducktape_rpc::Client,
    _ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let reply = client
            .status_json()
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn peers(
    client: ducktape_rpc::Client,
    _ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let reply = client.peers().await.map_err(|error| error.to_string())?;
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn blocks(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let limit = ask["limit"].as_u64().unwrap_or(0) as usize;
        let blocks = client
            .blocks(limit.clamp(1, MAX_BLOCKS))
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_vec(&blocks).map_err(|error| error.to_string())
    })
}

fn submit(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let payload = serde_json::to_vec(&ask["payload"]).map_err(|error| error.to_string())?;
        let height = crate::backend::seated_write(&client, &target, payload).await?;
        Ok(height.to_string().into_bytes())
    })
}

/// `{route, payload}` read once: the `/v1` route to POST and the bytes to
/// sign with it. The route is an ABSOLUTE path in plain tokens and carries
/// no query — the signature covers exactly the string the request sends, so
/// anything that would have to be escaped is REFUSED rather than escaped,
/// the way [`stream_ask`] refuses one. A string payload is the body
/// verbatim (`/v1/log-filter` takes a bare filter); anything else is its
/// JSON.
fn admin_ask(ask: &serde_json::Value) -> Result<(String, Vec<u8>), String> {
    let route = ask["route"].as_str().unwrap_or_default();
    let plain_path = route.starts_with("/v1/")
        && !route.contains("..")
        && route
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:~".contains(&byte));
    if !plain_path {
        return Err("`rpc.admin` names no plain `/v1` route".into());
    }
    let body = match &ask["payload"] {
        serde_json::Value::String(text) => text.clone().into_bytes(),
        other => serde_json::to_vec(other).map_err(|error| error.to_string())?,
    };
    Ok((route.to_owned(), body))
}

/// One node-level POST under the SEATED key. The proof is the one
/// `ducktape node log-filter` mints — `signed_req::request_headers` over the
/// method, the path and the body, bound to this node's key — reached through
/// the app's own [`crate::backend::seated_request_headers`], so nothing here
/// signs anything itself. The node's operator gate is the decider: a key it
/// does not admit gets the node's refusal, not the kernel's.
fn admin(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let (route, body) = admin_ask(&ask)?;
        let node_key = crate::backend::node_public_key(client.origin()).await?;
        let signed = crate::backend::seated_request_headers("POST", &route, &node_key, &body)
            .await
            .ok_or_else(|| "`rpc.admin` needs the session key unlocked".to_owned())?;
        let mut request = reqwest::Client::new()
            .post(format!("{}{route}", client.origin()))
            .body(body);
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("could not reach the node: {error}"))?;
        let code = response.status();
        let text = response.text().await.unwrap_or_default();
        match code.is_success() {
            true => Ok(text.into_bytes()),
            false => Err(format!("{route} rejected ({code}): {text}")),
        }
    })
}

/// Who a view's host composer may complete an `@` to: one scope's members,
/// as the VIEW read them. The kernel keeps no roster of its own — the list
/// is the guest's, parked where the composer surface finds it.
fn host_roster(payload: &[u8]) -> Option<(String, Vec<crate::backend::ChatMember>)> {
    let ask: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let scope = ask["scope"].as_str().filter(|scope| !scope.is_empty())?;
    let members = ask["members"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| crate::backend::ChatMember {
                    key: row["key"].as_str().unwrap_or_default().to_owned(),
                    label: row["label"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some((scope.to_owned(), members))
}

/// The merge commit a git-backed module's wire demands the CLIENT compute:
/// built against a bare mirror of the node's `/<target>/<repo>` smart-HTTP
/// remote, its minimal pack landed in the blob store under the seated key.
/// A conflict writes NOTHING and answers with the paths.
fn git_merge(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let repo = ask["repo"].as_str().unwrap_or_default().to_owned();
        let ours = ask["ours"].as_str().unwrap_or_default().to_owned();
        let theirs = ask["theirs"].as_str().unwrap_or_default().to_owned();
        let message = ask["message"].as_str().unwrap_or_default().to_owned();
        let pinned = !ours.is_empty() && !theirs.is_empty();
        if !pinned {
            return Err("`git.merge` names no commits to merge".into());
        }
        let endpoint = client.origin().to_owned();
        let build = tokio::task::spawn_blocking(move || {
            crate::backend::build_git_merge(&endpoint, &target, &repo, &ours, &theirs, &message)
        })
        .await
        .map_err(|error| format!("merge build task failed: {error}"))??;
        let (merge_oid, pack) = match build {
            crate::backend::MergeBuild::Conflicts(paths) => {
                let reply = serde_json::json!({ "conflicts": paths });
                return serde_json::to_vec(&reply).map_err(|error| error.to_string());
            }
            crate::backend::MergeBuild::Clean { merge_oid, pack } => (merge_oid, pack),
        };
        // The pack lands under the person's own signature, the same key the
        // op the view submits over it is signed with.
        let signer = crate::backend::seated_data_plane_signer(&client).await?;
        let pack_digest = client
            .with_write_auth(signer)
            .put_blob(pack)
            .await
            .map_err(|error| error.to_string())?
            .to_lowercase();
        let reply = serde_json::json!({ "merge_oid": merge_oid, "pack_digest": pack_digest });
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn picture_put(
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let surface = picture_surface(&ask).ok_or("`picture.put` names no surface")?;
        let path = ask["path"].as_str().unwrap_or_default().to_owned();
        if path.is_empty() {
            return Err("`picture.put` names no path".into());
        }
        // Each page is padded base64 in its own right, so the runs are
        // decoded separately and the BYTES joined — concatenating the text
        // would re-frame the stream at the first page boundary.
        let mut bytes = Vec::new();
        for page in ask["pages"].as_array().cloned().unwrap_or_default() {
            let page = crate::backend::base64_decode(page.as_str().unwrap_or_default())
                .ok_or_else(|| "`picture.put` page is not valid base64".to_owned())?;
            bytes.extend_from_slice(&page);
        }
        let (width, height) = crate::backend::store_picture(surface, path, bytes).await?;
        let reply = serde_json::json!({ "width": width, "height": height });
        serde_json::to_vec(&reply).map_err(|error| error.to_string())
    })
}

fn picture_inline(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let doc = ask["doc"].as_str().unwrap_or_default().to_owned();
        let source = ask["source"].as_str().unwrap_or_default().to_owned();
        let base = ask["base"].as_str().unwrap_or_default().to_owned();
        let net = ask["net"].as_str().unwrap_or_default().to_owned();
        if doc.is_empty() {
            return Err("`picture.inline` names no document".into());
        }
        crate::backend::load_inline_pictures(&client, doc, &source, base, net).await;
        Ok(Vec::new())
    })
}

/// The plane every block moves: a view that reads the feed itself
/// subscribes to it.
const BLOCK_PLANE: &str = "block";

/// A block moved `plane` (a module's, or [`BLOCK_PLANE`]): every `rpc.live`
/// subscription on it, in whichever view holds it, gets one item. Answers
/// `serial + 1` when a view was told — the app keeps that number in its
/// state, so the redraw that delivers the item follows — and `serial` when
/// none was.
pub fn live_hit(plane: &str, serial: i64) -> i64 {
    // lock order, everywhere: registry, then a view
    let registry = super::registry().lock().expect("module views");
    let mut told = false;
    for mounted in registry.values() {
        let mut locked = mounted.lock().expect("module view lock");
        let Slot::Ready(guest) = &mut locked.slot else {
            continue;
        };
        let ids: Vec<u64> = guest
            .live_subscriptions
            .iter()
            .filter(|(_, subscribed)| subscribed == plane)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            guest.pending.push(wire::Event::Response {
                id,
                result: Ok(b"{}".to_vec()),
                done: false,
            });
            told = true;
        }
    }
    match told {
        true => serial + 1,
        false => serial,
    }
}

/// The node's height as the app last heard it: a height that moved is a
/// hit on [`BLOCK_PLANE`]; the same height again, or none, is not.
pub fn block_hit(height: i64, serial: i64) -> i64 {
    static LAST: Mutex<i64> = Mutex::new(-1);
    let mut last = LAST.lock().expect("last height");
    let moved = height >= 0 && height != *last;
    if !moved {
        return serial;
    }
    *last = height;
    drop(last);
    live_hit(BLOCK_PLANE, serial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Message;

    /// The two strings a `rpc.stream` ask becomes, and what it refuses: the
    /// query is built ONCE here because the signature covers exactly the
    /// string the upgrade carries, and anything that would have to be
    /// escaped to survive that round trip is refused instead of escaped.
    #[test]
    fn a_stream_ask_becomes_one_topic_and_one_query_or_a_refusal() {
        let ask = serde_json::json!({
            "topic": "run-output:d4c3",
            "params": {"run": "d4c3"},
        });
        assert_eq!(
            stream_ask(&ask).expect("a plain ask"),
            ("run-output:d4c3".to_owned(), "?run=d4c3".to_owned())
        );
        assert_eq!(
            stream_ask(&serde_json::json!({"topic": "logs"})).expect("no params"),
            ("logs".to_owned(), String::new())
        );
        assert!(stream_ask(&serde_json::json!({"params": {"run": "a"}})).is_err());
        assert!(stream_ask(&serde_json::json!({"topic": ""})).is_err());
        assert!(stream_ask(&serde_json::json!({"topic": "a b"})).is_err());
        let smuggled = serde_json::json!({"topic": "logs", "params": {"run": "a&admin=1"}});
        assert!(
            stream_ask(&smuggled).is_err(),
            "a param that would need escaping is refused, never escaped"
        );
    }

    /// Every frame the node sends reaches the view verbatim, the kernel
    /// reading none of it, and the node's close is the item that ends the
    /// subscription — nothing after it is forwarded, and the in-flight count
    /// the open took comes back, which is what the test waits on.
    #[test]
    fn a_node_stream_forwards_every_frame_and_the_close_ends_it() {
        let replies = std::sync::Arc::new(Replies::default());
        replies.in_flight.fetch_add(1, Ordering::SeqCst);
        let frames = futures::stream::iter(vec![
            Ok(Message::Text(r#"{"topic":"run-output:d4c3"}"#.to_owned())),
            Ok(Message::Ping(Vec::new())),
            Ok(Message::Binary(vec![7, 8])),
            Ok(Message::Close(None)),
            Ok(Message::Text("after the close".to_owned())),
        ]);
        let running = replies.clone();
        let counted = InFlight(replies.clone());
        runtime().spawn(async move {
            let _counted = counted;
            forward(&running, 7, frames).await;
        });

        replies.wait_idle();
        let mut landed = Vec::new();
        replies.drain_into(&mut landed).expect("reply budget");
        let items: Vec<(u64, Result<Vec<u8>, String>, bool)> = landed
            .into_iter()
            .map(|event| match event {
                wire::Event::Response { id, result, done } => (id, result, done),
                other => panic!("the stream delivered {other:?}"),
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (7, Ok(br#"{"topic":"run-output:d4c3"}"#.to_vec()), false),
                (7, Ok(vec![7, 8]), false),
                (7, Ok(Vec::new()), true),
            ]
        );
        assert!(!replies.any_in_flight());
    }

    /// The route and the body a `rpc.admin` ask becomes, and what it
    /// refuses: the signature covers exactly the path string the POST
    /// carries, so a route that is not a plain absolute `/v1` path is
    /// refused instead of escaped. A string payload is the body verbatim —
    /// `/v1/log-filter` takes a bare filter, not JSON.
    #[test]
    fn an_admin_ask_becomes_one_route_and_one_body_or_a_refusal() {
        let ask = serde_json::json!({"route": "/v1/log-filter", "payload": "info,ducktape::join=debug"});
        assert_eq!(
            admin_ask(&ask).expect("a plain ask"),
            (
                "/v1/log-filter".to_owned(),
                b"info,ducktape::join=debug".to_vec()
            )
        );
        let structured = serde_json::json!({"route": "/v1/invite", "payload": {"ttl": 60}});
        assert_eq!(
            admin_ask(&structured).expect("a json ask"),
            ("/v1/invite".to_owned(), br#"{"ttl":60}"#.to_vec())
        );
        assert!(admin_ask(&serde_json::json!({"payload": "info"})).is_err());
        assert!(admin_ask(&serde_json::json!({"route": "v1/log-filter"})).is_err());
        assert!(admin_ask(&serde_json::json!({"route": "/v1/../admin/keys"})).is_err());
        let smuggled = serde_json::json!({"route": "/v1/log-filter?admin=1"});
        assert!(
            admin_ask(&smuggled).is_err(),
            "a route that would need escaping is refused, never escaped"
        );
    }

    /// A refused ask never reaches the node, and the answer lands in
    /// [`Replies`] like every other: the test waits on the in-flight count,
    /// never on a clock.
    #[test]
    fn a_refused_admin_ask_lands_as_one_answer_and_reaches_no_node() {
        let replies = std::sync::Arc::new(Replies::default());
        replies.in_flight.fetch_add(1, Ordering::SeqCst);
        let running = replies.clone();
        // port 1 is nothing's: a call that reached the network here would
        // fail with a transport error instead of the refusal asserted below
        let client = ducktape_rpc::Client::new("http://127.0.0.1:1").expect("a client");
        runtime().spawn(async move {
            let result = admin(client, serde_json::json!({"route": "/etc/passwd"})).await;
            running.deliver(11, result);
        });

        replies.wait_idle();
        let mut landed = Vec::new();
        replies.drain_into(&mut landed).expect("reply budget");
        assert_eq!(
            landed,
            vec![wire::Event::Response {
                id: 11,
                result: Err("`rpc.admin` names no plain `/v1` route".into()),
                done: true,
            }]
        );
        assert!(!replies.any_in_flight());
    }

    /// AN ANSWER THAT BEAT THE REDRAW THAT ASKED FOR IT IS STILL OWED A
    /// FRAME. The in-flight count is given back the moment the answer is
    /// written, so a node quick enough to answer inside the redraw leaves
    /// the count at zero with the answer undrained — and a caller reading
    /// only the count walks away from it, which is a view stuck on
    /// "Loading…" until something unrelated wakes it.
    #[test]
    fn an_answer_already_written_is_owed_a_frame_with_nothing_in_flight() {
        let replies = std::sync::Arc::new(Replies::default());
        assert!(!replies.answer_owed(), "nothing asked, nothing owed");

        replies.in_flight.fetch_add(1, Ordering::SeqCst);
        let running = replies.clone();
        // port 1 is nothing's: the refusal is composed without a node, which
        // is what makes this answer land inside the caller's own redraw
        let client = ducktape_rpc::Client::new("http://127.0.0.1:1").expect("a client");
        runtime().spawn(async move {
            let result = admin(client, serde_json::json!({"route": "/etc/passwd"})).await;
            running.deliver(3, result);
        });
        replies.wait_idle();

        assert!(!replies.any_in_flight(), "the count came back");
        assert!(replies.answer_owed(), "and the answer is still here");
        let mut landed = Vec::new();
        replies.drain_into(&mut landed).expect("reply budget");
        assert_eq!(landed.len(), 1);
        assert!(!replies.answer_owed(), "drained, and nothing is owed");
    }

    /// A subscription the view abandons is aborted mid-wait — a socket
    /// waiting on the node stops no other way — and the in-flight count it
    /// took comes back with it. It must, or the widget polls for a stream
    /// nobody is reading for the rest of the process.
    #[test]
    fn a_cancelled_stream_gives_back_the_count_it_took() {
        let replies = std::sync::Arc::new(Replies::default());
        replies.in_flight.fetch_add(1, Ordering::SeqCst);
        let running = replies.clone();
        let counted = InFlight(replies.clone());
        let waiting = runtime().spawn(async move {
            let _counted = counted;
            forward(
                &running,
                7,
                futures::stream::pending::<
                    Result<Message, tokio_tungstenite::tungstenite::Error>,
                >(),
            )
            .await;
        });
        assert!(replies.any_in_flight());

        drop(NodeStream(waiting));
        replies.wait_idle();
        let mut landed = Vec::new();
        replies.drain_into(&mut landed).expect("reply budget");
        assert!(landed.is_empty(), "an abort delivers nothing: {landed:?}");
    }
}
