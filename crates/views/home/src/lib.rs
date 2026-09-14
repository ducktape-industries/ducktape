//! The Home dashboard: one screen over the whole network, rendered from a
//! wasm component the desktop app loads off the registry — a `Kind::View`
//! entry with no core module of its own.
//!
//! The kernel pushes session facts only (`home.props`: connected, dark, the
//! chain id, the seated account). Everything drawn is read here through the
//! kernel's doors — `rpc.status`, `rpc.peers`, `rpc.blocks`, `rpc.query`,
//! `rpc.view`, `files.get` — and re-read on every `rpc.live` hit for the
//! plane that owns it. Every card is a summary and a door: a room or a run
//! opens its own tab through `home.open_link` carrying a `duck://` address
//! the shell's link plane routes. No op is ever addressed to this view and
//! it submits nothing.
//!
//! The page answers the pane it is drawn in: a sensor measures it, and the
//! cards stand in one, two or three columns by that width.
pub mod host;
mod presentation;

use std::collections::BTreeMap;

use ducktape_view_guest::{Subscription, Task};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HomeView {
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    pub(crate) chain: String,
    pub(crate) account: String,
    pub(crate) connection_serial: i64,
    /// the pane the dashboard was last measured at: what decides how many
    /// columns the cards stand in
    pub(crate) viewport_width: f64,
    pub(crate) facts: host::NodeFacts,
    pub(crate) peers: Vec<host::PeerRow>,
    pub(crate) blocks: Vec<host::BlockRow>,
    pub(crate) roster: host::Roster,
    pub(crate) rooms: Vec<host::RoomRow>,
    /// the head each room was first read at: what "new" is measured from
    pub(crate) rooms_seen: BTreeMap<String, i64>,
    pub(crate) files: Vec<host::SnapshotRow>,
    pub(crate) runs: Vec<host::RunRow>,
    pub(crate) proposals: Vec<host::ProposalRow>,
    /// each card's own reading error, by card key; empty is no error
    pub(crate) errors: BTreeMap<String, String>,
    pub(crate) host_error: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    SessionArrived(host::SessionItem),
    NodeArrived(host::NodeItem),
    RosterArrived(host::RosterItem),
    RoomsArrived(host::RoomsItem),
    FilesArrived(host::FilesItem),
    RunsArrived(host::RunsItem),
    ProposalsArrived(host::ProposalsItem),
    OpenRoom(String),
    OpenRun(String),
    CopyToClipboard(String, String),
    /// the pane measured, by the sensor around the cards
    ViewportChanged(f64, f64),
}

impl HomeView {
    const SNAPSHOT_SCHEMA: &'static str =
        "7a4c1e9b3d5f2a8c6e0b4d7f9a1c3e5b8d0f2a4c6e8b1d3f5a7c9e0b2d4f6a8c";

    fn state() -> Self {
        Self {
            connected: false,
            dark: false,
            chain: String::new(),
            account: String::new(),
            connection_serial: 0,
            viewport_width: host::UNMEASURED_WIDTH,
            facts: host::NodeFacts::default(),
            peers: Vec::new(),
            blocks: Vec::new(),
            roster: host::Roster::default(),
            rooms: Vec::new(),
            rooms_seen: BTreeMap::new(),
            files: Vec::new(),
            runs: Vec::new(),
            proposals: Vec::new(),
            errors: BTreeMap::new(),
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
            return Err("invalid home snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid home snapshot state".into());
        };
        wire::decode(&state)
    }

    /// The chain a link is spelled for: the session's, or the node's own
    /// report when the session names none.
    pub(crate) fn chain_id(&self) -> &str {
        match self.chain.is_empty() {
            true => &self.facts.chain_id,
            false => &self.chain,
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let session = host::session().map(Message::SessionArrived);
        if !self.connected {
            return session;
        }
        let serial = self.connection_serial;
        Subscription::batch([
            session,
            host::node(serial).map(Message::NodeArrived),
            host::roster(serial).map(Message::RosterArrived),
            host::rooms(serial).map(Message::RoomsArrived),
            host::files(serial).map(Message::FilesArrived),
            host::runs(serial).map(Message::RunsArrived),
            host::proposals(serial).map(Message::ProposalsArrived),
        ])
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::NodeArrived(item) => self.on_node_arrived(item),
            Message::RosterArrived(item) => self.on_roster_arrived(item),
            Message::RoomsArrived(item) => self.on_rooms_arrived(item),
            Message::FilesArrived(item) => self.on_files_arrived(item),
            Message::RunsArrived(item) => self.on_runs_arrived(item),
            Message::ProposalsArrived(item) => self.on_proposals_arrived(item),
            Message::OpenRoom(channel) => self.on_open_room(channel),
            Message::OpenRun(dispatch_id) => self.on_open_run(dispatch_id),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::ViewportChanged(width, height) => self.on_viewport_changed(width, height),
        }
    }

    fn on_session_arrived(&mut self, item: host::SessionItem) -> Task<Message> {
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
        self.account = next.account;
        Task::none()
    }

    /// One card's reading lands in its own slot and its own error line: a
    /// module this network does not run leaves its card empty, not the page.
    fn card_read(&mut self, card: &str, error: String) -> bool {
        let failed = !error.is_empty();
        match failed {
            true => {
                self.errors.insert(card.to_owned(), error);
            }
            false => {
                self.errors.remove(card);
            }
        }
        !failed
    }

    /// The three node readings ride one plane and land one at a time, each
    /// in its own card.
    fn on_node_arrived(&mut self, item: host::NodeItem) -> Task<Message> {
        match item {
            host::NodeItem::Facts(item) => self.on_facts_arrived(*item),
            host::NodeItem::Peers(item) => self.on_peers_arrived(item),
            host::NodeItem::Blocks(item) => self.on_blocks_arrived(item),
        }
    }

    fn on_facts_arrived(&mut self, item: host::FactsItem) -> Task<Message> {
        if self.card_read("node", item.error) {
            self.facts = item.facts;
        }
        Task::none()
    }

    fn on_peers_arrived(&mut self, item: host::PeersItem) -> Task<Message> {
        if self.card_read("peers", item.error) {
            self.peers = item.rows;
        }
        Task::none()
    }

    fn on_blocks_arrived(&mut self, item: host::BlocksItem) -> Task<Message> {
        if self.card_read("blocks", item.error) {
            self.blocks = item.rows;
        }
        Task::none()
    }

    fn on_viewport_changed(&mut self, width: f64, _height: f64) -> Task<Message> {
        self.viewport_width = width;
        Task::none()
    }

    fn on_roster_arrived(&mut self, item: host::RosterItem) -> Task<Message> {
        if self.card_read("members", item.error) {
            self.roster = item.roster;
        }
        Task::none()
    }

    fn on_rooms_arrived(&mut self, item: host::RoomsItem) -> Task<Message> {
        if self.card_read("rooms", item.error) {
            self.rooms_seen = host::baseline_after_read(&item.rows, &self.rooms_seen);
            self.rooms = item.rows;
        }
        Task::none()
    }

    fn on_files_arrived(&mut self, item: host::FilesItem) -> Task<Message> {
        if self.card_read("files", item.error) {
            self.files = item.rows;
        }
        Task::none()
    }

    fn on_runs_arrived(&mut self, item: host::RunsItem) -> Task<Message> {
        if self.card_read("runs", item.error) {
            self.runs = item.rows;
        }
        Task::none()
    }

    fn on_proposals_arrived(&mut self, item: host::ProposalsItem) -> Task<Message> {
        if self.card_read("proposals", item.error) {
            self.proposals = item.rows;
        }
        Task::none()
    }

    /// Following a room's link is reading it: its baseline moves to its
    /// head, so the news marker clears until the room moves again.
    fn on_open_room(&mut self, channel: String) -> Task<Message> {
        if let Some(room) = self.rooms.iter().find(|room| room.id == channel) {
            self.rooms_seen.insert(channel.clone(), room.head_seq);
        }
        host::open_link(&host::duck_channel_link(&channel, self.chain_id()));
        Task::none()
    }

    fn on_open_run(&mut self, dispatch_id: String) -> Task<Message> {
        host::open_link(&host::duck_run_link(&dispatch_id, self.chain_id()));
        Task::none()
    }

    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<Message> {
        host::copy(&text, &label);
        Task::none()
    }
}

ducktape_view_guest::export_app!(
    HomeView,
    "Home",
    "The network at a glance: this node, its peers, blocks, modules, members, rooms, files, runs and open decisions.",
    ["home"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_uses_the_host_envelope_and_rejects_invalid_state() {
        use ducktape_view_guest::wire;
        let (app, _) = HomeView::boot();
        let mut envelope = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        assert_eq!(envelope.schema, HomeView::SNAPSHOT_SCHEMA);
        envelope.schema = "0".repeat(64);
        assert!(HomeView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = HomeView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(HomeView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn snapshot_preserves_the_room_baselines() {
        let (mut app, _) = HomeView::boot();
        app.rooms_seen.insert("general".into(), 7);
        let snapshot = app.snapshot().unwrap();
        assert_eq!(
            HomeView::restore(&snapshot).unwrap().snapshot().unwrap(),
            snapshot
        );
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = HomeView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
