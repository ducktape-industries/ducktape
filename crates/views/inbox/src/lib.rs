//! The Inbox: every notification this account was sent, worded and judged
//! here, rendered from a wasm component the desktop app loads off the
//! registry.
//!
//! The kernel pushes session facts only (`inbox.props`: connected, dark, the
//! chain id, the seated account). WHAT IS UNREAD and WHAT EACH NOTIFICATION
//! SAYS are this view's own fold — read through the kernel's doors off the
//! inbox queue, the identity directory and each source module's read lane,
//! re-read on every `rpc.live` hit for the inbox plane. A row's door out is
//! `inbox.open_link` carrying a `duck://` address the shell's link plane
//! routes; "Mark all read" leaves as `op.submit`.
//!
//! The app keeps no second fold: the number beside its bell is this view's
//! count, answered through a headless background session.
pub mod host;
mod presentation;

use ducktape_view_guest::{Subscription, Task};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InboxView {
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    pub(crate) chain: String,
    pub(crate) account: String,
    pub(crate) connection_serial: i64,
    /// the visible page, worded
    pub(crate) rows: Vec<host::Row>,
    /// what the badge says: every unread item the rule does not silence
    pub(crate) unread: i64,
    /// the watermark "mark all read" signs
    pub(crate) head: i64,
    /// the queue has not answered yet on this connection
    pub(crate) reading: bool,
    /// a write is in flight: the button says so and refuses a second press
    pub(crate) marking: bool,
    /// the reading's own error; empty is no error
    pub(crate) error: String,
    pub(crate) host_error: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    SessionArrived(host::SessionItem),
    InboxArrived(host::InboxItem),
    BackgroundFinished,
    MarkAllRead,
    Marked(Result<(), String>),
    Open(String),
}

impl InboxView {
    /// This state's layout, digested — `snapshot_schema` holds it here.
    const SNAPSHOT_SCHEMA: &'static str =
        "9749634cb7ebb91b94c10b3b80d1f235509a4a817f2b6d554978f7e176def59e";

    fn state() -> Self {
        Self {
            connected: false,
            dark: false,
            chain: String::new(),
            account: String::new(),
            connection_serial: 0,
            rows: Vec::new(),
            unread: 0,
            head: 0,
            reading: false,
            marking: false,
            error: String::new(),
            host_error: String::new(),
        }
    }

    pub(crate) fn boot() -> (Self, Task<Message>) {
        (Self::state(), Task::none())
    }

    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";

    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        use ducktape_view_guest::wire;
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        use ducktape_view_guest::wire;
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid inbox snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid inbox snapshot state".into());
        };
        wire::decode(&state)
    }

    /// The queue is read once a session seats an account on a connected node.
    fn reads_the_queue(&self) -> bool {
        self.connected && !self.account.is_empty()
    }

    fn subscription(&self) -> Subscription<Message> {
        let session = host::session().map(Message::SessionArrived);
        if !self.reads_the_queue() {
            return session;
        }
        Subscription::batch([
            session,
            host::inbox(
                self.connection_serial,
                self.account.clone(),
                self.chain.clone(),
            )
            .map(Message::InboxArrived),
        ])
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::InboxArrived(item) => self.on_inbox_arrived(item),
            Message::BackgroundFinished => self.on_background_finished(),
            Message::MarkAllRead => self.on_mark_all_read(),
            Message::Marked(result) => self.on_marked(result),
            Message::Open(link) => self.on_open(link),
        }
    }

    /// A props document is either the errand a headless run was started for
    /// or the session facts a drawn tab gets.
    fn on_session_arrived(&mut self, item: host::SessionItem) -> Task<Message> {
        if let Some(errand) = item.background {
            return Task::perform(host::run_background(errand), |()| {
                Message::BackgroundFinished
            });
        }
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        let next = item.next;
        self.connection_serial =
            host::connection_serial_after(self.connected, next.connected, self.connection_serial);
        self.connected = next.connected;
        self.dark = next.dark;
        self.chain = next.chain;
        let account_moved = self.account != next.account;
        self.account = next.account;
        if account_moved {
            self.rows = Vec::new();
            self.unread = 0;
            self.head = 0;
            self.marking = false;
        }
        self.reading = self.reads_the_queue() && self.rows.is_empty() && self.error.is_empty();
        Task::none()
    }

    fn on_inbox_arrived(&mut self, item: host::InboxItem) -> Task<Message> {
        self.reading = false;
        self.error = item.error;
        if !self.error.is_empty() {
            return Task::none();
        }
        self.rows = item.rows;
        self.unread = item.unread;
        self.head = item.head;
        Task::none()
    }

    fn on_background_finished(&mut self) -> Task<Message> {
        Task::none()
    }

    /// The watermark is the queue's head, not the newest row on screen: an
    /// item this view silences is still read once you have been here.
    fn on_mark_all_read(&mut self) -> Task<Message> {
        let nothing_to_mark = self.unread <= 0 || self.head <= 0;
        if self.marking || nothing_to_mark {
            return Task::none();
        }
        self.marking = true;
        self.error = String::new();
        Task::perform(
            host::mark_read(self.account.clone(), self.head),
            Message::Marked,
        )
    }

    /// The write's outcome. The read watermark is the chain's, so the rows
    /// are not touched here: the commit lands on the inbox plane and the
    /// subscription re-reads.
    fn on_marked(&mut self, result: Result<(), String>) -> Task<Message> {
        self.marking = false;
        self.error = result.err().unwrap_or_default();
        Task::none()
    }

    fn on_open(&mut self, link: String) -> Task<Message> {
        host::open_link(&link);
        Task::none()
    }
}

ducktape_view_guest::export_app!(
    InboxView,
    "Inbox",
    "Every mention, assignment and result addressed to you, newest first.",
    ["inbox"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_uses_the_host_envelope_and_rejects_invalid_state() {
        use ducktape_view_guest::wire;
        let (app, _) = InboxView::boot();
        let mut envelope = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        assert_eq!(envelope.schema, InboxView::SNAPSHOT_SCHEMA);
        envelope.schema = "0".repeat(64);
        assert!(InboxView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = InboxView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(InboxView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = InboxView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod snapshot_schema {
    use super::InboxView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<InboxView>(InboxView::SNAPSHOT_SCHEMA, |_| {});
    }
}
