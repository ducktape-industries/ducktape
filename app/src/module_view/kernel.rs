//! The kernel contract: what EVERY module view may ask of the app, with no
//! per-module code on this side of the wire. A view that speaks only this
//! contract is replaced by a module deployment alone — the app binary
//! never changes for it.
//!
//! - `net.request` `{account, route, method, path, headers, body}` — an
//!   application HTTP request through the signed gateway route. Methods use
//!   lowercase names, body is a byte array; the reply is the gateway envelope
//!   `{head: {status, headers}, body_b64}`. The host resolves the publisher and
//!   revision and signs with the seated key. No raw endpoint or caller proof
//!   comes from the view.
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
//! - `net.stream` — the same logical route request as `net.request`, using
//!   a bodyless GET; opens a bidirectional application stream. Items are JSON
//!   `{text: "..."}` or `{binary: [0, 1]}`, preserving the WebSocket frame type.
//!   A terminal empty item ends the stream. Cancel retires the socket with its
//!   view instance.
//! - `net.send` `{stream, frame: {close: null}}` closes gracefully; otherwise
//!   `{stream, frame: {text: "..."}}` or
//!   `{stream, frame: {binary: [0, 1]}}` — queue one frame on this guest
//!   instance's stream. Success acknowledges queue admission; application
//!   replies arrive on the stream. Full queues refuse immediately.
//! - `session.start` `{view, props}` — start a companion view as a subscription.
//!   One actual native button activation permits one start; generated events
//!   and restored data cannot supply activation. At most four companions run
//!   per parent. `session.send` `{session, props}` updates its bounded inputs.
//!   Cancelling the subscription or replacing its parent retires the companion.
//! - `host.emit` raw bytes publishes at most 16 KiB per item, queue 16, from
//!   an active companion. `host.finish` empty bytes drains queued output then
//!   ends it and releases its tasks/devices. Navigation preserves the instance.
//! - `media.audio` subscribes to 960-sample mono 48 kHz PCM capture;
//!   `media.play` `{audio,samples}` plays one frame, `media.mute`
//!   `{audio,muted}` controls capture. `media.video` `{source,max_bytes}`
//!   subscribes to camera/screen JPEG capture, including an opaque `preview`
//!   image key for local display. `media.image` allocates an
//!   opaque image key; `media.put` `{image,jpeg}` updates it and `media.drop`
//!   `{image}` releases it. All resource IDs belong to one active user-started
//!   companion; the host interprets no room or peer protocol.
//! - `rpc.query_as_reader` `{target, query}` — the query AS THE SEATED
//!   KEY'S HOLDER (`/v1/query/reader`), for a module that serves protected
//!   content to its reader; refused while the key is locked.
//! - `blob.get` `{digest, limit}` — a blob by hex digest, verified against it.
//! - `picture.load` `{surface, path}` — a duckfs file paged in, decoded and
//!   parked in a host picture surface's slot; answered with its drawn size.
//! - `blob.put` `<raw bytes>` — a blob landed on the node, proven with the seated key;
//!   answered with the digest.
//! - `op.submit_bytes` `{target, body_b64, required_blob?}` — exact binary module payload.
//! - `op.submit` `{target, payload, required_blob?}` — one JSON module op, signed with the
//!   SEATED key and submitted; answered with the block height. The view
//!   never carries a password, an endpoint or a key. `required_blob` is a
//!   64-character lowercase SHA256 hex digest bound into the signed frame.
//! - `rpc.admin` `{route, payload}` — one POST to a `/v1` route that
//!   mutates THE NODE rather than module state, signed with the SEATED key
//!   exactly as the `ducktape node` verbs sign theirs; answered with the
//!   node's own reply text, or its refusal. The kernel names no route —
//!   the node's operator gate decides what this key may ask for.
//! - `picture.put` `{surface, path, pages}` — base64 pages decoded, joined
//!   and parked as the picture the `picture` surface draws under `surface`;
//!   answered `{width, height}`. `picture.inline` `{doc, source, base, net}`
//!   — the pictures a Markdown `source` embeds, resolved against the
//!   document's own `duck://` address and parked under `doc` for the
//!   document surface. Both are the app's decoder and its one outbound
//!   picture gate, which a view has neither of.
//! - `host.visible` empty bytes subscribes to JSON booleans: whether this view
//!   is presented in a shell tab. Hiding delivers one bounded update; queued
//!   responses precede the next visible event. Cancellation and replacement
//!   retire the subscription with its guest instance.
//! - `host.badge` `<count>` — the tab badge, handed to the app as the
//!   `badge` event with `{"count": N}` in its detail.
//! - `asset.read` `<canonical-relative-path>` — exact bytes of an asset in
//!   this guest's verified deployment, at most 1 MiB. Missing or invalid paths
//!   are refused; the operation never reads disk or fetches from the network.
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

pub(super) mod media;

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
/// One queued and one writing frame per stream, each at most 64 KiB.
const MAX_STREAM_SEND_BYTES: usize = 64 << 10;
const MAX_IN_FLIGHT: usize = 256;
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_REPLY_EVENTS: usize = 1024;
const MAX_REPLY_BYTES: usize = 32 << 20;
/// The share of the reply budget SUBSCRIPTIONS may fill before they stop
/// reading their sources. A request answers once, so the budget bounds it
/// on its own; a subscription answers forever, against a queue only a
/// redraw empties — the node's `logs` topic replays its whole ring (4,096
/// frames) the moment a view subscribes, and a view the user has switched
/// away from is not redrawing at all. Without a park that producer walks
/// straight into the fault above and stops the view for good. Half, so the
/// answers a redraw is actually waiting on keep the other half.
const MAX_STREAM_BACKLOG_EVENTS: usize = MAX_REPLY_EVENTS / 2;
const MAX_STREAM_BACKLOG_BYTES: usize = MAX_REPLY_BYTES / 2;

/// What one answer holds against the reply budget.
fn result_bytes(result: &Result<Vec<u8>, String>) -> usize {
    match result {
        Ok(bytes) => bytes.len(),
        Err(error) => error.len(),
    }
}

/// What the queue holds against it.
fn queued_bytes(events: &[wire::Event]) -> usize {
    events
        .iter()
        .map(|event| match event {
            wire::Event::Response { result, .. } => result_bytes(result),
            _ => 0,
        })
        .sum()
}

/// The kernel's answers to a view's requests, written off-thread and
/// drained into the guest's pending events at its next redraw.
pub(super) struct Replies {
    events: Mutex<Vec<wire::Event>>,
    in_flight: AtomicUsize,
    /// Told on every answer delivered: a test waits here for the node
    /// calls in flight, never on a clock.
    landed: std::sync::Condvar,
    changed: tokio::sync::watch::Sender<()>,
    /// Told on every redraw that takes the queue: a subscription parked on
    /// [`Replies::backlogged`] wakes here and reads its socket again.
    drained: tokio::sync::watch::Sender<()>,
    fault: Mutex<Option<String>>,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            events: Mutex::default(),
            in_flight: AtomicUsize::new(0),
            landed: std::sync::Condvar::new(),
            changed: tokio::sync::watch::channel(()).0,
            drained: tokio::sync::watch::channel(()).0,
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
        self.drained.send_replace(());
        Ok(())
    }

    /// Told on every drain: what a parked subscription waits on.
    fn drains(&self) -> tokio::sync::watch::Receiver<()> {
        self.drained.subscribe()
    }

    /// Whether ONE subscription's share of the queue is spoken for, in
    /// either budget — a frame past this waits for a redraw rather than
    /// growing the queue toward the fault in [`Replies::item`].
    fn backlogged(&self) -> bool {
        let events = self.events.lock().expect("kernel replies");
        events.len() >= MAX_STREAM_BACKLOG_EVENTS
            || queued_bytes(&events) >= MAX_STREAM_BACKLOG_BYTES
    }

    /// One item from a SUBSCRIPTION, which is the only producer that can
    /// outrun the redraw: it answers for as long as the view holds it,
    /// against a queue only a redraw empties. It PARKS here while its share
    /// is spoken for, so its own source — a node socket, a companion
    /// session — holds the backlog instead of the queue growing into the
    /// fault. `false` when the view is gone and the subscription should end.
    async fn subscription_item(
        &self,
        drained: &mut tokio::sync::watch::Receiver<()>,
        id: u64,
        result: Result<Vec<u8>, String>,
    ) -> bool {
        while self.backlogged() {
            if drained.changed().await.is_err() {
                return false;
            }
        }
        self.item(id, result, false);
        self.fault().is_none()
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
        let queued = queued_bytes(&events);
        let exceeds_budget = events.len() >= MAX_REPLY_EVENTS
            || result_bytes(&result) > MAX_REPLY_BYTES.saturating_sub(queued);
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

    /// A reply writer can outlive its guest; tests deliver to that exact queue.
    #[cfg(test)]
    pub(super) fn inject_item(&self, id: u64, result: Result<Vec<u8>, String>) {
        self.item(id, result, true);
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
    if super::filesystem::answer(guest, capability, operation, id, payload) {
        return true;
    }
    if capability == "media" {
        media::answer(guest, operation, id, payload);
        return true;
    }
    if capability == "session" {
        session_answer(guest, operation, id, payload);
        return true;
    }
    match (capability, operation) {
        ("host", "visible") => {
            if !payload.is_empty() {
                guest.refuse(id, "visibility subscription takes no payload".into());
                return true;
            }
            guest.visibility_subscriptions.push(id);
            guest.pending.push(wire::Event::Response {
                id,
                result: Ok(guest.visible.to_string().into_bytes()),
                done: false,
            });
        }
        ("rpc", "query") => spawn(guest, id, payload, query),
        ("net", "request") => spawn(guest, id, payload, application_call),
        ("rpc", "view") => spawn(guest, id, payload, view),
        ("rpc", "blocks") => spawn(guest, id, payload, blocks),
        ("rpc", "status") => spawn(guest, id, b"{}", status),
        ("rpc", "peers") => spawn(guest, id, b"{}", peers),
        ("rpc", "query_as_reader") => spawn(guest, id, payload, query_as_reader),
        ("rpc", "stream") => stream_open(guest, id, payload),
        ("net", "send") => stream_send(guest, id, payload),
        ("net", "stream") => application_stream(guest, id, payload),
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
        ("asset", "read") => asset_read(guest, id, payload),
        ("op", "submit") => spawn(guest, id, payload, submit),
        ("op", "submit_bytes") => spawn(guest, id, payload, submit_bytes),
        ("rpc", "admin") => spawn(guest, id, payload, admin),
        ("picture", "put") => spawn_host(guest, id, payload, picture_put),
        ("picture", "inline") => spawn(guest, id, payload, picture_inline),
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
fn asset_read(guest: &mut Guest, id: u64, payload: &[u8]) {
    const MAX_ASSET_READ_BYTES: usize = 1 << 20;
    let result = std::str::from_utf8(payload)
        .map_err(|_| "asset path is not UTF-8".to_owned())
        .and_then(|path| {
            module_artifact::validate_asset_path(path)?;
            let bytes = super::artifact_asset(&guest.assets, path)
                .ok_or_else(|| "asset is absent from this deployment".to_owned())?;
            let oversized = bytes.len() > MAX_ASSET_READ_BYTES;
            if oversized {
                return Err("asset exceeds read byte limit".into());
            }
            Ok(bytes.to_vec())
        });
    guest.reply(id, result);
}

fn spawn(guest: &mut Guest, id: u64, payload: &[u8], call: Call) {
    let ask: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(ask) => ask,
        Err(error) => {
            guest.refuse(id, format!("request is not JSON: {error}"));
            return;
        }
    };
    let client = match node_client(guest) {
        Ok(client) => client,
        Err(error) => {
            guest.refuse(id, error);
            return;
        }
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let task = runtime().spawn(async move {
        let _counted = counted;
        let result = call(client, ask).await;
        replies.item(id, result, true);
    });
    guest
        .tasks
        .retain(|(_, pending)| !pending.task.is_finished());
    guest.tasks.push((
        id,
        NodeTask {
            task,
            outgoing: None,
        },
    ));
}

/// A companion view is a resource of its user-started subscription. Hidden
/// navigation preserves the parent Guest; cancellation or replacement drops it.
fn session_answer(guest: &mut Guest, operation: &str, id: u64, payload: &[u8]) {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Start {
        view: String,
        props: serde_json::Value,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Send {
        session: u64,
        props: serde_json::Value,
    }
    match operation {
        "start" => {
            guest.sessions.retain(|_, input| !input.is_closed());
            guest
                .tasks
                .retain(|(_, pending)| !pending.task.is_finished());
            if guest.user_activation.take().is_none() {
                guest.refuse(id, "session.start requires a native button action".into());
                return;
            }
            let ask: Start = match serde_json::from_slice(payload) {
                Ok(ask) => ask,
                Err(error) => {
                    guest.refuse(id, error.to_string());
                    return;
                }
            };
            let full = guest.sessions.len() >= 4;
            if full {
                guest.refuse(id, "at most four companion sessions may run".into());
                return;
            }
            let session = match super::background::start_at(
                &ask.view,
                serde_json::to_vec(&ask.props).expect("JSON properties"),
                guest.connection_rev,
            ) {
                Ok(session) => session,
                Err(error) => {
                    guest.refuse(id, error);
                    return;
                }
            };
            guest.sessions.insert(id, session.input);
            let replies = guest.replies.clone();
            let task = runtime().spawn(async move {
                use futures::StreamExt as _;
                let mut drained = replies.drains();
                let mut events = session.events;
                while let Some(event) = events.next().await {
                    if !replies.subscription_item(&mut drained, id, event).await {
                        return;
                    }
                }
                replies.item(id, Ok(Vec::new()), true);
            });
            guest.tasks.push((
                id,
                NodeTask {
                    task,
                    outgoing: None,
                },
            ));
        }
        "send" => {
            let result = serde_json::from_slice::<Send>(payload)
                .map_err(|error| error.to_string())
                .and_then(|ask| {
                    let session = guest
                        .sessions
                        .get(&ask.session)
                        .ok_or("unknown session resource")?;
                    let props =
                        serde_json::to_vec(&ask.props).map_err(|error| error.to_string())?;
                    if props.len() > 16 * 1024 {
                        return Err("session properties exceed 16 KiB".into());
                    }
                    session
                        .send(props)
                        .map_err(|_| "session closed".to_owned())?;
                    Ok(Vec::new())
                });
            guest.reply(id, result);
        }
        _ => guest.refuse(id, "unknown session operation".into()),
    }
}

/// One bounded, cancellable device operation owned by the requesting guest.
pub(super) fn spawn_device(
    guest: &mut Guest,
    id: u64,
    future: impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static,
) {
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let task = runtime().spawn(async move {
        let _counted = counted;
        replies.item(id, future.await, true);
    });
    guest
        .tasks
        .retain(|(_, pending)| !pending.task.is_finished());
    guest.tasks.push((
        id,
        NodeTask {
            task,
            outgoing: None,
        },
    ));
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
    let task = runtime().spawn(async move {
        let _counted = counted;
        let result = call(ask).await;
        replies.item(id, result, true);
    });
    guest
        .tasks
        .retain(|(_, pending)| !pending.task.is_finished());
    guest.tasks.push((
        id,
        NodeTask {
            task,
            outgoing: None,
        },
    ));
}

fn node_client(guest: &Guest) -> Result<ducktape_rpc::Client, String> {
    client_for_revision(guest.connection_rev)
}

fn client_for_revision(revision: u64) -> Result<ducktape_rpc::Client, String> {
    let connection = super::connection().lock().expect("views rpc");
    let same_network = connection.rev == revision;
    if !same_network {
        return Err("view belongs to a previous network connection".into());
    }
    connection
        .client
        .clone()
        .ok_or_else(|| "not connected to a node".into())
}

type RawCall = fn(
    ducktape_rpc::Client,
    Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>;

/// [`spawn`] for a request whose payload is the bytes themselves.
fn spawn_raw(guest: &mut Guest, id: u64, payload: &[u8], call: RawCall) {
    let client = match node_client(guest) {
        Ok(client) => client,
        Err(error) => {
            guest.refuse(id, error);
            return;
        }
    };
    let bytes = payload.to_vec();
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let task = runtime().spawn(async move {
        let _counted = counted;
        let result = call(client, bytes).await;
        replies.item(id, result, true);
    });
    guest
        .tasks
        .retain(|(_, pending)| !pending.task.is_finished());
    guest.tasks.push((
        id,
        NodeTask {
            task,
            outgoing: None,
        },
    ));
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

pub(super) struct NodeTask {
    task: tokio::task::JoinHandle<()>,
    outgoing: Option<tokio::sync::mpsc::Sender<tokio_tungstenite::tungstenite::Message>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutboundFrame {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamSend {
    stream: u64,
    frame: OutboundFrame,
}

fn outbound(payload: &[u8]) -> Result<(u64, tokio_tungstenite::tungstenite::Message), String> {
    use tokio_tungstenite::tungstenite::Message;
    // A JSON string can encode one byte as six bytes (\u0000).
    let oversized_request = payload.len() > MAX_STREAM_SEND_BYTES * 6 + 128;
    if oversized_request {
        return Err("stream send request is too large".into());
    }
    let ask: StreamSend = serde_json::from_slice(payload).map_err(|error| error.to_string())?;
    let frame = match ask.frame {
        OutboundFrame::Text(text) => Message::Text(text),
        OutboundFrame::Binary(bytes) => Message::Binary(bytes),
        OutboundFrame::Close => Message::Close(None),
    };
    let oversized_frame = frame.len() > MAX_STREAM_SEND_BYTES;
    if oversized_frame {
        return Err("stream send frame is too large".into());
    }
    Ok((ask.stream, frame))
}

fn stream_send(guest: &mut Guest, id: u64, payload: &[u8]) {
    if let Err(error) = node_client(guest) {
        guest.refuse(id, error);
        return;
    }
    let result = outbound(payload).and_then(|(stream, frame)| {
        let (_, socket) = guest
            .tasks
            .iter()
            .find(|(owned, _)| *owned == stream)
            .ok_or("stream is not open in this view instance")?;
        socket
            .outgoing
            .as_ref()
            .ok_or("request is not a bidirectional stream")?
            .try_send(frame)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => "stream send queue is full",
                tokio::sync::mpsc::error::TrySendError::Closed(_) => "stream is closed",
            })?;
        Ok(Vec::new())
    });
    guest.replies.item(id, result, true);
}

async fn exchange<S>(
    replies: &Replies,
    id: u64,
    socket: S,
    mut outgoing: tokio::sync::mpsc::Receiver<tokio_tungstenite::tungstenite::Message>,
) where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + futures::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + Unpin,
{
    use futures::{SinkExt as _, StreamExt as _};
    let (mut writer, reader) = socket.split();
    let send = async {
        while let Some(frame) = outgoing.recv().await {
            let closing = frame.is_close();
            tokio::time::timeout(std::time::Duration::from_secs(30), writer.send(frame))
                .await
                .map_err(|_| "application stream send timed out".to_owned())?
                .map_err(|error| format!("node stream send failed: {error}"))?;
            if closing {
                return Ok(Vec::new());
            }
        }
        Ok(Vec::new())
    };
    tokio::select! {
        () = forward(replies, id, reader, StreamEncoding::Frames) => {},
        result = send => replies.item(id, result, true),
    }
}

impl Drop for NodeTask {
    fn drop(&mut self) {
        self.task.abort();
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
    guest.tasks.retain(|(_, stream)| !stream.task.is_finished());
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
    let client = match node_client(guest) {
        Ok(client) => client,
        Err(error) => {
            guest.refuse(id, error);
            return;
        }
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let handle = runtime().spawn(async move {
        let _counted = counted;
        match open_topic(client.origin(), &topic, &query).await {
            Ok(socket) => forward(&replies, id, socket, StreamEncoding::Bytes).await,
            Err(error) => replies.item(id, Err(error), true),
        }
    });
    guest.tasks.push((
        id,
        NodeTask {
            task: handle,
            outgoing: None,
        },
    ));
}

fn application_stream(guest: &mut Guest, id: u64, payload: &[u8]) {
    guest.tasks.retain(|(_, stream)| !stream.task.is_finished());
    let request = match serde_json::from_slice(payload)
        .map_err(|error| error.to_string())
        .and_then(application_request)
    {
        Ok(request) => request,
        Err(error) => {
            guest.refuse(id, error);
            return;
        }
    };
    let bodyless_get = request.method == gateway::RouteMethod::Get && request.body.is_empty();
    if !bodyless_get {
        guest.refuse(id, "application stream requires a bodyless GET".into());
        return;
    }
    let client = match node_client(guest) {
        Ok(client) => client,
        Err(error) => {
            guest.refuse(id, error);
            return;
        }
    };
    let replies = guest.replies.clone();
    let Some(counted) = replies.admit() else {
        guest.refuse(id, "too many in-flight view requests".into());
        return;
    };
    let (outgoing, receiver) = tokio::sync::mpsc::channel(1);
    let task = runtime().spawn(async move {
        let _counted = counted;
        let open = async {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
            let head = application_head(&client, &request, true).await?;
            let encoded = gateway::encode_proxy_request_head(&head)?;
            let url = crate::backend::agent_ws_url(client.origin());
            let origin = url
                .strip_suffix("/v1/ws")
                .ok_or("invalid node websocket origin")?;
            let mut request = format!("{origin}/v1/gateway/stream")
                .into_client_request()
                .map_err(|_| "invalid application stream destination")?;
            request.headers_mut().insert(
                "x-ducktape-gateway-head",
                encoded
                    .try_into()
                    .map_err(|_| "invalid application stream head")?,
            );
            let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                max_message_size: Some(MAX_STREAM_FRAME_BYTES),
                max_frame_size: Some(MAX_STREAM_FRAME_BYTES),
                ..Default::default()
            };
            // A guest sends one whole application message per `net.send`, so
            // Nagle has nothing to coalesce on this socket — it only holds a
            // frame back until the previous one is acknowledged, adding a
            // round trip to every frame on a long link. A view cannot reach a
            // socket option and should not be able to; the bounded transport
            // the host offers is where the property belongs.
            let (socket, _) =
                tokio_tungstenite::connect_async_with_config(request, Some(config), true)
                    .await
                    .map_err(|_| "application stream transport failed")?;
            Ok::<_, String>(socket)
        };
        match tokio::time::timeout(std::time::Duration::from_secs(30), open).await {
            Ok(Ok(socket)) => exchange(&replies, id, socket, receiver).await,
            Ok(Err(error)) => replies.item(id, Err(error), true),
            Err(_) => replies.item(id, Err("application stream open timed out".into()), true),
        }
    });
    guest.tasks.push((
        id,
        NodeTask {
            task,
            outgoing: Some(outgoing),
        },
    ));
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
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(MAX_STREAM_FRAME_BYTES),
        max_frame_size: Some(MAX_STREAM_FRAME_BYTES),
        ..Default::default()
    };
    // The node pushes one committed event per frame down this socket and
    // nothing on it is ever coalesced into a bigger write, so Nagle can only
    // hold a frame back until the previous one is acknowledged — a round trip
    // per event on an app pointed at a node that is not this machine's.
    let (mut socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), true)
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
#[derive(Clone, Copy)]
enum StreamEncoding {
    Bytes,
    Frames,
}

async fn forward<S>(replies: &Replies, id: u64, mut socket: S, encoding: StreamEncoding)
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
    let mut drained = replies.drains();
    while let Some(message) = socket.next().await {
        let frame = match message {
            Ok(frame @ (Message::Text(_) | Message::Binary(_))) => frame,
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
        let bytes = match (encoding, frame) {
            (StreamEncoding::Bytes, Message::Text(text)) => text.into_bytes(),
            (StreamEncoding::Bytes, Message::Binary(bytes)) => bytes,
            (StreamEncoding::Frames, Message::Text(text)) => {
                serde_json::to_vec(&OutboundFrame::Text(text)).expect("text frame encodes")
            }
            (StreamEncoding::Frames, Message::Binary(bytes)) => {
                serde_json::to_vec(&OutboundFrame::Binary(bytes)).expect("binary frame encodes")
            }
            _ => unreachable!("only data frames reach the encoder"),
        };
        // The socket is where a topic that outruns the redraw waits: reading
        // on regardless would grow the queue into the fault that stops the
        // view, and the node's `logs` topic replays 4,096 frames the moment
        // a view subscribes — four times the whole budget, so that fault was
        // a certainty, not a corner.
        if !replies.subscription_item(&mut drained, id, Ok(bytes)).await {
            return;
        }
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
    use crate::backend::{CHAT_SURFACE, FILES_SURFACE, FORGE_SURFACE, PAGES_SURFACE};
    match ask["surface"].as_str()? {
        FILES_SURFACE => Some(FILES_SURFACE),
        FORGE_SURFACE => Some(FORGE_SURFACE),
        CHAT_SURFACE => Some(CHAT_SURFACE),
        PAGES_SURFACE => Some(PAGES_SURFACE),
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

/// A view names a logical route. Publisher, revision and caller identity are
/// resolved or signed by the host, never accepted from guest bytes.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationRequest {
    account: u64,
    route: Option<String>,
    method: gateway::RouteMethod,
    path: String,
    #[serde(default)]
    headers: Vec<gateway::ProxyHeader>,
    #[serde(default)]
    body: Vec<u8>,
}

fn application_request(value: serde_json::Value) -> Result<ApplicationRequest, String> {
    let request: ApplicationRequest =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    gateway::validate_proxy_request_head(&gateway::ProxyRequestHead {
        operator: false,
        account_id: request.account,
        name: gateway::RouteName {
            label: request.route.clone(),
        },
        revision: 1,
        method: request.method,
        path_and_query: request.path.clone(),
        headers: request.headers.clone(),
        body_len: request.body.len() as u64,
        upgrade: false,
        user_pop: None,
    })?;
    Ok(request)
}

async fn application_head(
    client: &ducktape_rpc::Client,
    request: &ApplicationRequest,
    upgrade: bool,
) -> Result<gateway::ProxyRequestHead, String> {
    let name = gateway::RouteName {
        label: request.route.clone(),
    };
    let reply: gateway::GatewayReply = client
        .query(
            "gateway",
            &gateway::GatewayQuery::Get {
                account_id: request.account,
                name: name.clone(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let gateway::GatewayReply::Route(record) = reply else {
        return Err("gateway returned an unexpected route reply".into());
    };
    let record = record.ok_or("application route is not published")?;
    let active = record.statement.route.is_some();
    if !active {
        return Err("application route is not published".into());
    }
    let mut head = gateway::ProxyRequestHead {
        operator: false,
        account_id: request.account,
        name,
        revision: record.statement.revision,
        method: request.method,
        path_and_query: request.path.clone(),
        headers: request.headers.clone(),
        body_len: request.body.len() as u64,
        upgrade,
        user_pop: None,
    };
    head.user_pop = Some(
        crate::backend::seated_gateway_proof(
            &record.statement.publisher_node,
            &head,
            &request.body,
        )
        .await
        .ok_or("application request needs the session key unlocked")?,
    );
    Ok(head)
}

fn application_call(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        use base64::Engine as _;
        let request = application_request(ask)?;
        let head = application_head(&client, &request, false).await?;
        let body = serde_json::json!({"head":head,
            "body_b64":base64::engine::general_purpose::STANDARD.encode(request.body)});
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        let mut response = http
            .post(format!("{}/v1/gateway/proxy", client.origin()))
            .json(&body)
            .send()
            .await
            .map_err(|_| "application request transport failed")?;
        let status = response.status();
        let mut bytes = Vec::new();
        // The proxy envelope base64-encodes the bounded upstream response.
        let limit = gateway::MAX_RESPONSE_BODY_BYTES as usize * 4 / 3 + (64 << 10);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "application response transport failed")?
        {
            let exceeds_limit = bytes.len().saturating_add(chunk.len()) > limit;
            if exceeds_limit {
                return Err("application response exceeds the view limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!("application route refused request ({status})"));
        }
        Ok(bytes)
    })
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

fn required_blob_of(ask: &serde_json::Value) -> Result<Option<[u8; 32]>, String> {
    let Some(value) = ask.get("required_blob") else {
        return Ok(None);
    };
    let digest = value
        .as_str()
        .ok_or("required_blob must be a SHA256 hex string")?;
    let canonical = digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !canonical {
        return Err("required_blob must be 64 lowercase hexadecimal characters".into());
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16)
            .map_err(|error| error.to_string())?;
    }
    Ok(Some(bytes))
}

/// The envelope names only a module and exact bytes; the seated signer owns
/// identity and sequence, just as it does for JSON operations.
fn submit_bytes(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let encoded = ask["body_b64"]
            .as_str()
            .ok_or("body_b64 must be a string")?;
        let payload = crate::backend::base64_decode(encoded).ok_or("invalid operation base64")?;
        let height =
            crate::backend::seated_write(&client, &target, payload, required_blob_of(&ask)?)
                .await?;
        Ok(height.to_string().into_bytes())
    })
}

fn submit(
    client: ducktape_rpc::Client,
    ask: serde_json::Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>> {
    Box::pin(async move {
        let target = target_of(&ask)?;
        let payload = serde_json::to_vec(&ask["payload"]).map_err(|error| error.to_string())?;
        let height =
            crate::backend::seated_write(&client, &target, payload, required_blob_of(&ask)?)
                .await?;
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
        let content_type = match &ask["payload"] {
            serde_json::Value::String(_) => "text/plain; charset=utf-8",
            _ => "application/json",
        };
        let mut request = reqwest::Client::new()
            .post(format!("{}{route}", client.origin()))
            .header(reqwest::header::CONTENT_TYPE, content_type)
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

fn live_events() -> &'static tokio::sync::broadcast::Sender<String> {
    static EVENTS: std::sync::OnceLock<tokio::sync::broadcast::Sender<String>> =
        std::sync::OnceLock::new();
    EVENTS.get_or_init(|| tokio::sync::broadcast::channel(128).0)
}

pub(super) fn isolated_live_events() -> tokio::sync::broadcast::Receiver<String> {
    live_events().subscribe()
}

pub(super) fn invalidate_live(guest: &mut Guest, plane: Option<&str>) {
    for (id, subscribed) in &guest.live_subscriptions {
        if plane.is_none_or(|plane| plane == subscribed) {
            guest.pending.push(wire::Event::Response {
                id: *id,
                result: Ok(b"{}".to_vec()),
                done: false,
            });
        }
    }
}

/// A block moved `plane` (a module's, or [`BLOCK_PLANE`]): every `rpc.live`
/// subscription on it, in whichever view holds it, gets one item. Answers
/// `serial + 1` when a view was told — the app keeps that number in its
/// state, so the redraw that delivers the item follows — and `serial` when
/// none was.
pub fn live_hit(plane: &str, serial: i64) -> i64 {
    let _ = live_events().send(plane.to_owned());
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

    #[test]
    fn blob_prerequisite_accepts_only_an_exact_digest() {
        assert_eq!(required_blob_of(&serde_json::json!({})).unwrap(), None);
        let valid = serde_json::json!({"required_blob": "ab".repeat(32)});
        assert_eq!(required_blob_of(&valid).unwrap(), Some([0xab; 32]));
        for invalid in [
            serde_json::json!(null),
            serde_json::json!(23),
            serde_json::json!("ab"),
            serde_json::json!("gg".repeat(32)),
            serde_json::json!("AB".repeat(32)),
            serde_json::json!("ab".repeat(33)),
        ] {
            assert!(required_blob_of(&serde_json::json!({"required_blob": invalid})).is_err());
        }
    }

    #[tokio::test]
    async fn old_view_cannot_continue_requests_on_a_new_network() {
        let _turn = super::super::tests::connection_turn().await;
        let revision = {
            let mut connection = super::super::connection().lock().unwrap();
            connection.client = Some(ducktape_rpc::Client::new("http://127.0.0.1:3001").unwrap());
            connection.rev
        };
        let captured = client_for_revision(revision).unwrap();
        {
            let mut connection = super::super::connection().lock().unwrap();
            connection.rev += 1;
            connection.client = Some(ducktape_rpc::Client::new("http://127.0.0.1:3002").unwrap());
        }
        assert!(client_for_revision(revision).is_err());
        assert_eq!(captured.origin(), "http://127.0.0.1:3001");
        assert_eq!(
            client_for_revision(revision + 1).unwrap().origin(),
            "http://127.0.0.1:3002"
        );
    }

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
            forward(&running, 7, frames, StreamEncoding::Bytes).await;
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

    /// A subscription that outruns the redraw PARKS instead of stopping the
    /// view, and every frame still arrives. The node's `logs` topic replays
    /// its whole ring the moment a view subscribes — four times the reply
    /// budget — so a forwarder that reads on regardless walks the view into
    /// the backlog fault before the first redraw ever runs.
    #[test]
    fn a_stream_longer_than_the_reply_budget_parks_instead_of_stopping_the_view() {
        const FRAMES: usize = MAX_REPLY_EVENTS * 4;
        let replies = Replies::default();
        type Frame = Result<Message, tokio_tungstenite::tungstenite::Error>;
        let frames = futures::stream::iter(
            (0..FRAMES)
                .map(|nth| Message::Text(format!("line-{nth}")))
                .map(Frame::Ok),
        );
        let mut landed = Vec::new();
        let mut forwarding = std::pin::pin!(forward(&replies, 7, frames, StreamEncoding::Bytes));
        // The redraw, and nothing else, is what lets the forwarder read on:
        // every park here is answered with one drain, so the run is the
        // whole handshake with no thread and no clock in it.
        futures::executor::block_on(std::future::poll_fn(|cx| {
            if std::future::Future::poll(forwarding.as_mut(), cx).is_ready() {
                return std::task::Poll::Ready(());
            }
            let before = landed.len();
            replies
                .drain_into(&mut landed)
                .expect("the budget holds against a stream four times its size");
            assert!(
                landed.len() > before,
                "the forwarder parked on a queue the redraw had already emptied"
            );
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }));
        replies
            .drain_into(&mut landed)
            .expect("the budget holds against a stream four times its size");
        assert!(replies.fault().is_none(), "{:?}", replies.fault());
        assert_eq!(landed.len(), FRAMES + 1, "every frame, then the close");
        assert!(matches!(
            landed.last(),
            Some(wire::Event::Response { done: true, .. })
        ));
    }

    #[test]
    fn application_requests_name_logical_routes_and_refuse_host_authority() {
        let valid = serde_json::json!({"account": 42, "route": "canvas", "method": "post", "path": "/stroke", "body": [1,2]});
        let request = application_request(valid.clone()).unwrap();
        assert_eq!(request.account, 42);
        assert_eq!(request.body, vec![1, 2]);
        for path in [
            "http://127.0.0.1/private",
            "//elsewhere/private",
            "/ok\r\nInjected: true",
        ] {
            let mut bad = valid.clone();
            bad["path"] = path.into();
            assert!(application_request(bad).is_err());
        }
        for field in ["publisher", "user_pop", "endpoint"] {
            let mut bad = valid.clone();
            bad[field] = "forged".into();
            assert!(application_request(bad).is_err());
        }
        let mut bad = valid;
        bad["headers"] = serde_json::json!([{"name":"x-duck-caller-account", "value":"1"}]);
        assert!(application_request(bad).is_err());
    }

    #[test]
    fn stream_sends_validate_frames_and_bound_the_queue() {
        let (stream, frame) = outbound(br#"{"stream":7,"frame":{"text":"hello"}}"#).unwrap();
        assert_eq!(stream, 7);
        assert_eq!(frame, Message::Text("hello".into()));
        let (_, binary) = outbound(br#"{"stream":7,"frame":{"binary":[0,255]}}"#).unwrap();
        assert_eq!(binary, Message::Binary(vec![0, 255]));
        assert_eq!(
            outbound(br#"{"stream":7,"frame":{"close":null}}"#)
                .unwrap()
                .1,
            Message::Close(None)
        );
        assert!(outbound(br#"{"stream":7,"frame":{"text":"ok"},"endpoint":"elsewhere"}"#).is_err());
        let at_limit =
            serde_json::json!({"stream":7, "frame":{"text":"x".repeat(MAX_STREAM_SEND_BYTES)}});
        assert!(outbound(&serde_json::to_vec(&at_limit).unwrap()).is_ok());
        let oversized =
            serde_json::json!({"stream":7, "frame":{"text":"x".repeat(MAX_STREAM_SEND_BYTES + 1)}});
        assert!(outbound(&serde_json::to_vec(&oversized).unwrap()).is_err());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        sender.try_send(frame).unwrap();
        assert!(matches!(
            sender.try_send(binary.clone()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        receiver.try_recv().unwrap();
        sender.try_send(binary.clone()).unwrap();
        drop(receiver);
        assert!(matches!(
            sender.try_send(binary),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
        ));
    }

    #[test]
    fn a_node_stream_exchanges_frames_and_peer_close_stops_the_writer() {
        use futures::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
        runtime().block_on(async {
            let (client, server) = tokio::io::duplex(4096);
            let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let replies = std::sync::Arc::new(Replies::default());
            let running = replies.clone();
            let task = tokio::spawn(async move { exchange(&running, 7, client, receiver).await });
            sender.send(Message::Text("request".into())).await.unwrap();
            assert_eq!(server.next().await.unwrap().unwrap(), Message::Text("request".into()));
            server.send(Message::Binary(vec![9, 8])).await.unwrap();
            sender.send(Message::Binary(vec![1, 2])).await.unwrap();
            assert_eq!(server.next().await.unwrap().unwrap(), Message::Binary(vec![1, 2]));
            server.close(None).await.unwrap();
            task.await.unwrap();
            assert!(sender.is_closed());
            let mut events = Vec::new();
            replies.drain_into(&mut events).unwrap();
            assert_eq!(events.len(), 2);
            assert!(matches!(&events[0], wire::Event::Response { id: 7, result: Ok(bytes), done: false } if bytes == br#"{"binary":[9,8]}"#));
            assert!(matches!(&events[1], wire::Event::Response { id: 7, result: Ok(bytes), done: true } if bytes.is_empty()));
        });
    }

    #[test]
    fn closing_an_application_stream_flushes_close_and_finishes_once() {
        use futures::StreamExt as _;
        use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
        runtime().block_on(async {
            let (client, server) = tokio::io::duplex(4096);
            let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let replies = std::sync::Arc::new(Replies::default());
            let running = replies.clone();
            let task = tokio::spawn(async move { exchange(&running, 7, client, receiver).await });
            sender.send(Message::Close(None)).await.unwrap();
            assert_eq!(server.next().await.unwrap().unwrap(), Message::Close(None));
            task.await.unwrap();
            assert!(sender.is_closed());
            let mut events = Vec::new();
            replies.drain_into(&mut events).unwrap();
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], wire::Event::Response { id: 7, result: Ok(bytes), done: true } if bytes.is_empty()));
        });
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
                futures::stream::pending::<Result<Message, tokio_tungstenite::tungstenite::Error>>(
                ),
                StreamEncoding::Bytes,
            )
            .await;
        });
        assert!(replies.any_in_flight());

        let (outgoing, _receiver) = tokio::sync::mpsc::channel(1);
        drop(NodeTask {
            task: waiting,
            outgoing: Some(outgoing),
        });
        replies.wait_idle();
        let mut landed = Vec::new();
        replies.drain_into(&mut landed).expect("reply budget");
        assert!(landed.is_empty(), "an abort delivers nothing: {landed:?}");
    }
}
