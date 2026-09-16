//! The block explorer as a module-owned view on the kernel contract.
//!
//! The kernel pushes session facts only (`explorer.props`: connected, dark,
//! and the live head and sync line the app already holds). The block window
//! is this view's own — `rpc.blocks`, re-read on every `rpc.live` hit for the
//! `block` plane — and so is the workspace search, which fans out over
//! `rpc.query` and `rpc.view`. A clipboard copy is the one act that leaves as
//! an intent: the OS door is the kernel's. The endpoint, the key and the
//! password never cross.
pub mod host;
use ducktape_view_guest::{Subscription, Task};
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ExplorerView {
    pub(crate) connected: bool,
    pub(crate) loading: bool,
    pub(crate) blocks: Vec<crate::host::ExplorerBlock>,
    pub(crate) ops: Vec<crate::host::ExplorerOp>,
    pub(crate) head: i64,
    pub(crate) sync_line: String,
    pub(crate) ledger_serial: i64,
    pub(crate) hits: Vec<crate::host::ExplorerHit>,
    pub(crate) kinds: Vec<crate::host::KindCount>,
    pub(crate) partial: String,
    pub(crate) searching: bool,
    pub(crate) sent_query: String,
    pub(crate) search_serial: i64,
    pub(crate) query: String,
    pub(crate) kind: String,
    pub(crate) selected: i64,
    pub(crate) viewport_width: f64,
    pub(crate) ledger_width: f64,
    pub(crate) host_error: String,
    pub(crate) dark: bool,
}
#[derive(Clone, Debug)]
pub enum Message {
    SessionArrived(crate::host::SessionItem),
    LedgerArrived(crate::host::LedgerItem),
    SearchArrived(crate::host::SearchItem),
    Refresh,
    CopyToClipboard(String, String),
    SearchSubmit,
    ClearExplorerSearch,
    PickExplorerKind(String),
    SelectExplorerBlock(i64),
    LedgerResized(f64, f64),
    ViewportChanged(f64, f64),
    BindQuery(String),
}
impl ExplorerView {
    /// This state's layout, digested — `snapshot_schema` holds it here.
    const SNAPSHOT_SCHEMA: &'static str =
        "b953dcaf342cbc42a2d6b9b740f358974966958beb3af8cba6d354a1110a630f";
    fn state() -> Self {
        Self {
            connected: false,
            loading: false,
            blocks: Vec::new(),
            ops: Vec::new(),
            head: 0,
            sync_line: "".to_owned(),
            ledger_serial: 0,
            hits: Vec::new(),
            kinds: Vec::new(),
            partial: "".to_owned(),
            searching: false,
            sent_query: "".to_owned(),
            search_serial: 0,
            query: "".to_owned(),
            kind: "all".to_owned(),
            selected: 0,
            viewport_width: 1280.0,
            ledger_width: 340.0,
            host_error: "".to_owned(),
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, Task<Message>) {
        (Self::state(), Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        use ducktape_view_guest::wire;
        self.validate_snapshot()?;
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
            return Err("invalid explorer snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid explorer snapshot state".into());
        };
        let restored: Self = wire::decode(&state)?;
        restored.validate_snapshot()?;
        Ok(restored)
    }
    fn validate_snapshot(&self) -> Result<(), String> {
        let finite = self.viewport_width.is_finite() && self.ledger_width.is_finite();
        if !finite {
            return Err("non-finite explorer geometry".into());
        }
        Ok(())
    }
}

impl ExplorerView {
    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![host::session().map(Message::SessionArrived)];
        if !self.connected {
            return Subscription::batch(subscriptions);
        }
        subscriptions.push(host::ledger(self.ledger_serial).map(Message::LedgerArrived));
        if !self.sent_query.is_empty() {
            subscriptions.push(
                host::workspace_search(self.sent_query.clone(), self.search_serial)
                    .map(Message::SearchArrived),
            );
        }
        Subscription::batch(subscriptions)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_rejects_non_finite_geometry_on_both_sides() {
        use ducktape_view_guest::wire;
        let (mut app, _) = ExplorerView::boot();
        app.ledger_width = f64::NAN;
        assert!(app.snapshot().is_err());
        let bytes = wire::Snapshot {
            schema: ExplorerView::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(&app)),
        }.encode().unwrap();
        assert!(ExplorerView::restore(&bytes).is_err());
    }
    #[test]
    fn snapshot_uses_the_host_envelope_and_rejects_invalid_state() {
        use ducktape_view_guest::wire;
        let (app, _) = ExplorerView::boot();
        let mut envelope = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        assert_eq!(envelope.schema, ExplorerView::SNAPSHOT_SCHEMA);
        envelope.schema = "0".repeat(64);
        assert!(ExplorerView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = ExplorerView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(ExplorerView::restore(&envelope.encode().unwrap()).is_err());
    }
    #[test]
    fn snapshot_preserves_draft_selection_and_search_results() {
        let (mut app, _) = ExplorerView::boot();
        app.query = "한글 draft".into();
        app.sent_query = "previous query".into();
        app.kind = "page".into();
        app.selected = 42;
        app.ledger_width = 380.;
        app.hits.push(host::ExplorerHit {
            title: "A page".into(),
            ..Default::default()
        });
        let snapshot = app.snapshot().unwrap();
        let restored = ExplorerView::restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot);
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = ExplorerView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
impl ExplorerView {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::LedgerArrived(item) => self.on_ledger_arrived(item),
            Message::SearchArrived(item) => self.on_search_arrived(item),
            Message::Refresh => self.on_refresh(),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::SearchSubmit => self.on_search_submit(),
            Message::ClearExplorerSearch => self.on_clear_explorer_search(),
            Message::PickExplorerKind(next) => self.on_pick_explorer_kind(next),
            Message::SelectExplorerBlock(height) => self.on_select_explorer_block(height),
            Message::LedgerResized(dx, _dy) => self.on_ledger_resized(dx, _dy),
            Message::ViewportChanged(width, _height) => self.on_viewport_changed(width, _height),
            Message::BindQuery(value) => self.on_bind_query(value),
        }
    }
    fn on_session_arrived(&mut self, item: crate::host::SessionItem) -> Task<Message> {
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        let next = item.next.clone();
        self.head = next.head;
        self.sync_line = next.sync_line.to_owned();
        self.ledger_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.ledger_serial,
        );
        self.loading = crate::host::loading_after(self.connected, next.connected, self.loading);
        self.connected = next.connected;
        self.dark = next.dark;
        Task::none()
    }
    fn on_ledger_arrived(&mut self, item: crate::host::LedgerItem) -> Task<Message> {
        self.loading = false;
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        self.blocks = item.blocks;
        self.ops = item.ops;
        Task::none()
    }
    fn on_search_arrived(&mut self, item: crate::host::SearchItem) -> Task<Message> {
        self.searching = false;
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        self.hits = item.hits;
        self.kinds = item.kinds;
        self.partial = item.partial;
        Task::none()
    }
    fn on_refresh(&mut self) -> Task<Message> {
        if (!self.connected) || self.loading {
            return Task::none();
        }
        self.loading = true;
        self.ledger_serial += 1;
        Task::none()
    }
    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<Message> {
        host::copy(&text, &label);
        Task::none()
    }
    fn on_search_submit(&mut self) -> Task<Message> {
        let blocked = !self.connected || self.searching || self.query.trim().is_empty();
        if blocked {
            return Task::none();
        }
        self.kind = "all".to_owned();
        self.hits = Vec::new();
        self.kinds = Vec::new();
        self.partial = "".to_owned();
        self.searching = true;
        self.search_serial += 1;
        self.sent_query = self.query.trim().to_owned();
        Task::none()
    }
    fn on_clear_explorer_search(&mut self) -> Task<Message> {
        self.query = "".to_owned();
        self.kind = "all".to_owned();
        self.hits = Vec::new();
        self.kinds = Vec::new();
        self.partial = "".to_owned();
        self.searching = false;
        self.sent_query = "".to_owned();
        Task::none()
    }
    fn on_pick_explorer_kind(&mut self, next: String) -> Task<Message> {
        self.kind = next;
        Task::none()
    }
    fn on_select_explorer_block(&mut self, height: i64) -> Task<Message> {
        self.selected = height;
        Task::none()
    }
    fn on_ledger_resized(&mut self, dx: f64, _dy: f64) -> Task<Message> {
        self.ledger_width =
            crate::host::ledger_width_after_delta(self.ledger_width, dx, self.viewport_width);
        Task::none()
    }
    fn on_viewport_changed(&mut self, width: f64, _height: f64) -> Task<Message> {
        self.viewport_width = width;
        self.ledger_width = crate::host::ledger_width_after_delta(self.ledger_width, 0.0, width);
        Task::none()
    }
    fn on_bind_query(&mut self, value: String) -> Task<Message> {
        self.query = value;
        Task::none()
    }
}
mod presentation;
ducktape_view_guest::export_app!(
    ExplorerView,
    "Explorer",
    "The ledger this network wrote: blocks, their operations, and a search over the workspace.",
    ["explorer"]
);

#[cfg(test)]
mod snapshot_schema {
    use super::ExplorerView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<ExplorerView>(ExplorerView::SNAPSHOT_SCHEMA, |_| {});
    }
}
