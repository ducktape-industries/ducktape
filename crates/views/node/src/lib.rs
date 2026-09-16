//! This node's operator screen as a view on the kernel contract: status,
//! standing, peers, the log ring and the code registry, rendered from a
//! wasm component the desktop app loads from a file.
//!
//! The kernel pushes session facts only (`node.props`: connected, dark, this
//! seat's admin standing and tier, the app's connection reading, the
//! daemon's workspace directory and the wall clock). The node's own facts,
//! its peers and its code registry are read here through the kernel's
//! `rpc.status` / `rpc.peers` / `rpc.query`, re-read on every `rpc.live` hit
//! for the `block` plane, and the log ring arrives through `rpc.stream` on
//! the node's own `logs` topic — the timeline is this view's state, not the
//! app's. Retuning the running node's tracing filter leaves as one
//! `rpc.admin` POST the kernel signs with the seated key; the clipboard is
//! the one intent left, because it is an OS door.
pub mod host;
use ducktape_view_guest::{Subscription, Task};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum NodeTab {
    Overview,
    Permissions,
    Activity,
    Modules,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct NodeView {
    pub(crate) node_data_dir: String,
    pub(crate) tier: String,
    pub(crate) admin: bool,
    pub(crate) status: String,
    pub(crate) wall_now: i64,
    pub(crate) connected: bool,
    pub(crate) connection_serial: i64,
    pub(crate) node_tab: NodeTab,
    pub(crate) facts: crate::host::NodeFacts,
    pub(crate) module_rows: Vec<crate::host::ModuleRow>,
    pub(crate) node_peers: Vec<crate::host::PeerRow>,
    pub(crate) log_lines: Vec<crate::host::LogRow>,
    pub(crate) node_log_filter: String,
    /// the level the console keeps, "" for every level
    pub(crate) node_log_level: String,
    pub(crate) live_log_filter: String,
    /// what the last retune came to: in flight, the node's answer, or its
    /// refusal — and whether it was a refusal
    pub(crate) live_filter_note: String,
    pub(crate) live_filter_failed: bool,
    pub(crate) host_error: String,
    pub(crate) dark: bool,
}
#[derive(Clone, Debug)]
pub enum Message {
    SessionArrived(crate::host::SessionItem),
    FactsArrived(crate::host::FactsItem),
    PeersArrived(crate::host::PeersItem),
    ModulesArrived(crate::host::ModulesItem),
    LogsArrived(crate::host::LogItem),
    ActDone(crate::host::ActItem),
    SelectNodeTab(NodeTab),
    OpenNodeModules,
    NodeLogFilterChanged(String),
    SelectLogLevel(String),
    LiveLogFilterChanged(String),
    ApplyLiveLogFilter,
    CopyToClipboard(String, String),
}
impl NodeView {
    /// This state's layout, digested — `snapshot_schema` holds it here.
    const SNAPSHOT_SCHEMA: &'static str =
        "a3c71212f7dd37b4968727ff2aa8999979360e8d6a5c0ee634db40e8823988eb";
    fn state() -> Self {
        Self {
            node_data_dir: "".to_owned(),
            tier: "".to_owned(),
            admin: false,
            status: "".to_owned(),
            wall_now: 0,
            connected: false,
            connection_serial: 0,
            node_tab: NodeTab::Overview,
            facts: crate::host::empty_facts(),
            module_rows: Vec::new(),
            node_peers: Vec::new(),
            log_lines: Vec::new(),
            node_log_filter: "".to_owned(),
            node_log_level: "".to_owned(),
            live_log_filter: "".to_owned(),
            live_filter_note: "".to_owned(),
            live_filter_failed: false,
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
            return Err("invalid node snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid node snapshot state".into());
        };
        wire::decode(&state)
    }
}

impl NodeView {
    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            host::session().map(Message::SessionArrived),
            host::acts().map(Message::ActDone),
        ];
        if !self.connected {
            return Subscription::batch(subscriptions);
        }
        subscriptions.push(host::facts(self.connection_serial).map(Message::FactsArrived));
        let tab = match self.node_tab {
            NodeTab::Overview => host::peers(self.connection_serial).map(Message::PeersArrived),
            NodeTab::Permissions => Subscription::none(),
            NodeTab::Activity => host::logs(self.connection_serial).map(Message::LogsArrived),
            NodeTab::Modules => host::modules(self.connection_serial).map(Message::ModulesArrived),
        };
        subscriptions.push(tab);
        Subscription::batch(subscriptions)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_uses_the_host_envelope_and_rejects_invalid_state() {
        use ducktape_view_guest::wire;
        let (app, _) = NodeView::boot();
        let mut envelope = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        assert_eq!(envelope.schema, NodeView::SNAPSHOT_SCHEMA);
        envelope.schema = "0".repeat(64);
        assert!(NodeView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = NodeView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(NodeView::restore(&envelope.encode().unwrap()).is_err());
    }
    #[test]
    fn snapshot_preserves_activity_filters_and_the_log_ring() {
        let (mut app, _) = NodeView::boot();
        app.node_tab = NodeTab::Activity;
        app.node_log_filter = "joining".into();
        app.live_log_filter = "info,ducktape::join=debug".into();
        app.log_lines.push(host::LogRow {
            cursor: "42".into(),
            time: "now".into(),
            level: "WARN".into(),
            message: "joining".into(),
        });
        let snapshot = app.snapshot().unwrap();
        assert_eq!(
            NodeView::restore(&snapshot).unwrap().snapshot().unwrap(),
            snapshot
        );
    }
    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = NodeView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
impl NodeView {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::FactsArrived(item) => self.on_facts_arrived(item),
            Message::PeersArrived(item) => self.on_peers_arrived(item),
            Message::ModulesArrived(item) => self.on_modules_arrived(item),
            Message::LogsArrived(item) => self.on_logs_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::SelectNodeTab(next) => self.on_select_node_tab(next),
            Message::OpenNodeModules => self.on_open_node_modules(),
            Message::NodeLogFilterChanged(next) => self.on_node_log_filter_changed(next),
            Message::SelectLogLevel(level) => self.on_select_log_level(level),
            Message::LiveLogFilterChanged(next) => self.on_live_log_filter_changed(next),
            Message::ApplyLiveLogFilter => self.on_apply_live_log_filter(),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
        }
    }
    fn on_session_arrived(&mut self, item: crate::host::SessionItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        let next = item.next.clone();
        self.connection_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.connection_serial,
        );
        self.connected = next.connected;
        self.dark = next.dark;
        self.admin = next.admin;
        self.tier = next.tier;
        self.status = next.status;
        self.node_data_dir = next.data_dir;
        self.wall_now = next.wall_now;
        Task::none()
    }
    fn on_facts_arrived(&mut self, item: crate::host::FactsItem) -> Task<Message> {
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        self.facts = item.facts;
        Task::none()
    }
    fn on_peers_arrived(&mut self, item: crate::host::PeersItem) -> Task<Message> {
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        self.node_peers = item.rows;
        Task::none()
    }
    fn on_modules_arrived(&mut self, item: crate::host::ModulesItem) -> Task<Message> {
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        self.module_rows = item.rows;
        Task::none()
    }
    fn on_logs_arrived(&mut self, item: crate::host::LogItem) -> Task<Message> {
        self.host_error = item.error;
        self.log_lines = host::push_logs(&self.log_lines, &item.lines);
        Task::none()
    }
    /// The node's answer to a retune lands beside the control that asked,
    /// never in the page's error strip: the node echoes the filter it now
    /// runs, and a refusal is one sentence with a verb.
    fn on_act_done(&mut self, item: crate::host::ActItem) -> Task<Message> {
        self.live_filter_failed = !item.error.is_empty();
        self.live_filter_note = match self.live_filter_failed {
            true => format!("The node refused the filter: {}", item.error),
            false => format!("The node now logs at {}", item.reply),
        };
        Task::none()
    }
    fn on_select_node_tab(&mut self, next: NodeTab) -> Task<Message> {
        self.node_tab = next;
        Task::none()
    }
    fn on_open_node_modules(&mut self) -> Task<Message> {
        self.node_tab = NodeTab::Modules;
        Task::none()
    }
    fn on_node_log_filter_changed(&mut self, next: String) -> Task<Message> {
        self.node_log_filter = next;
        Task::none()
    }
    fn on_select_log_level(&mut self, level: String) -> Task<Message> {
        self.node_log_level = level;
        Task::none()
    }
    fn on_live_log_filter_changed(&mut self, next: String) -> Task<Message> {
        self.live_log_filter = next;
        Task::none()
    }
    fn on_apply_live_log_filter(&mut self) -> Task<Message> {
        if (!self.admin) || (self.live_log_filter).is_empty() {
            return Task::none();
        }
        self.live_filter_note = "Retuning the node…".to_owned();
        self.live_filter_failed = false;
        host::set_log_filter(&self.live_log_filter);
        Task::none()
    }
    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<Message> {
        host::copy(&text, &label);
        Task::none()
    }
}
mod presentation;
ducktape_view_guest::export_app!(
    NodeView,
    "Node",
    "This node: coherent status, standing, peers, logs and the code registry.",
    ["node"]
);

#[cfg(test)]
mod snapshot_schema {
    use super::NodeView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<NodeView>(NodeView::SNAPSHOT_SCHEMA, |_| {});
    }
}
