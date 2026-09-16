//! The command palette: the workspace search the reader summons with a
//! chord, rendered from a wasm component the desktop app loads.
//!
//! THE APP HAS NO PALETTE. It does not know which key opens this, what a
//! query searches, how a hit reads or where pressing one goes: the chord is
//! claimed at the kernel (`host.chord`), the messages and pages are this
//! view's own reads, and a hit leaves as `host.open_link`. What the app owns
//! is the seat this draws in — a view that draws nothing is a window with
//! nothing over it.
//!
//! A CLOSED PALETTE COSTS ONE EMPTY TREE. It holds no clock, asks the node
//! nothing and keeps no hits: the chord subscription is the only thing alive
//! while it is closed, and it is the host's own stream.
pub mod host;
mod presentation;

use ducktape_view_guest::{Subscription, Task};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PaletteView {
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    pub(crate) chain: String,
    /// open, and therefore drawing: the whole difference between a palette
    /// and nothing at all
    pub(crate) open: bool,
    /// what the reader has typed
    pub(crate) draft: String,
    /// the query the hits on screen answer
    pub(crate) query: String,
    /// bumped per opening, so the same query typed twice searches twice
    pub(crate) serial: i64,
    pub(crate) chat: Vec<host::ChatHit>,
    pub(crate) pages: Vec<host::PageHit>,
    /// a query is out and has not been answered
    pub(crate) searching: bool,
    /// the search's own error; empty is no error
    pub(crate) error: String,
    pub(crate) host_error: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    SessionArrived(host::SessionItem),
    ChordPressed,
    DraftChanged(String),
    SearchArrived(host::SearchItem),
    Open(String),
    Dismiss,
}

impl PaletteView {
    /// This state's layout, digested — `snapshot_schema` holds it here.
    const SNAPSHOT_SCHEMA: &'static str =
        "fe0aadacba195936a2bc5afb515180f65d93c531f849f9826b252f6bfcbfd195";

    fn state() -> Self {
        Self {
            connected: false,
            dark: false,
            chain: String::new(),
            open: false,
            draft: String::new(),
            query: String::new(),
            serial: 0,
            chat: Vec::new(),
            pages: Vec::new(),
            searching: false,
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
            return Err("invalid palette snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid palette snapshot state".into());
        };
        wire::decode(&state)
    }

    /// Is there a query worth asking the node? An empty draft searches
    /// nothing, and a closed palette asks nothing at all.
    fn searchable(&self) -> Option<String> {
        let query = self.draft.trim();
        let asks = self.open && self.connected && !query.is_empty();
        asks.then(|| query.to_owned())
    }

    fn subscription(&self) -> Subscription<Message> {
        let session = host::session().map(Message::SessionArrived);
        let chord = host::chord().map(|()| Message::ChordPressed);
        if !self.open {
            // A CLOSED PALETTE HOLDS NO CLOCK, NO READ AND NO KEY. Only the
            // session and the chord stand, and both are the host's own
            // streams — a window with no palette over it pays for nothing.
            return Subscription::batch([session, chord]);
        }
        // Escape closes what the chord opened. It is listened for only while
        // the palette is up, so nothing else's Escape is eaten.
        let escape = Subscription::filter_events(|event| {
            let ducktape_view_guest::wire::Event::Keyboard { event, .. } = event else {
                return None;
            };
            use ducktape_view_guest::wire::keyboard::{Event, Key, Named};
            let Event::Press { state, .. } = event else {
                return None;
            };
            let escape = state.key == Key::Named(Named::Escape);
            escape.then_some(Message::Dismiss)
        });
        let Some(query) = self.searchable() else {
            return Subscription::batch([session, chord, escape]);
        };
        Subscription::batch([
            session,
            chord,
            escape,
            host::search(query, self.serial).map(Message::SearchArrived),
        ])
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::ChordPressed => self.on_chord_pressed(),
            Message::DraftChanged(text) => self.on_draft_changed(text),
            Message::SearchArrived(item) => self.on_search_arrived(item),
            Message::Open(link) => self.on_open(link),
            Message::Dismiss => self.on_dismiss(),
        }
    }

    fn on_session_arrived(&mut self, item: host::SessionItem) -> Task<Message> {
        self.host_error = item.error;
        if !self.host_error.is_empty() {
            return Task::none();
        }
        let next = item.next;
        let left_the_network = self.connected && !next.connected;
        self.connected = next.connected;
        self.dark = next.dark;
        self.chain = next.chain;
        // What was found belonged to the network that answered for it.
        if left_the_network {
            return self.on_dismiss();
        }
        Task::none()
    }

    /// The chord: an open palette closes, a closed one opens empty. A claim
    /// the kernel refused never arrives as a press at all — the key belongs
    /// to another view, and the kernel names the holder in its own log — so
    /// a palette whose claim lost simply never opens.
    fn on_chord_pressed(&mut self) -> Task<Message> {
        match self.open {
            true => self.on_dismiss(),
            false => {
                self.open = true;
                self.serial = self.serial.wrapping_add(1);
                Task::none()
            }
        }
    }

    fn on_draft_changed(&mut self, text: String) -> Task<Message> {
        self.draft = text;
        self.searching = self.searchable().is_some();
        if !self.searching {
            self.query = String::new();
            self.chat = Vec::new();
            self.pages = Vec::new();
            self.error = String::new();
        }
        Task::none()
    }

    /// An answer is kept only while it still answers what is typed: the
    /// subscription is keyed on the query, so a late item can only be the
    /// one this draft asked for.
    fn on_search_arrived(&mut self, item: host::SearchItem) -> Task<Message> {
        if item.query != self.draft.trim() {
            return Task::none();
        }
        self.searching = false;
        self.query = item.query;
        self.chat = item.chat;
        self.pages = item.pages;
        self.error = item.error;
        Task::none()
    }

    /// A hit leaves through the kernel's one door, and the palette gets out
    /// of the way: what it was opened to find has been found.
    fn on_open(&mut self, link: String) -> Task<Message> {
        host::open_link(&link);
        self.on_dismiss()
    }

    fn on_dismiss(&mut self) -> Task<Message> {
        self.open = false;
        self.draft = String::new();
        self.query = String::new();
        self.chat = Vec::new();
        self.pages = Vec::new();
        self.searching = false;
        self.error = String::new();
        Task::none()
    }
}

ducktape_view_guest::export_app!(
    PaletteView,
    "Palette",
    "Search every message and page in this workspace from one field.",
    ["palette"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_uses_the_host_envelope_and_rejects_invalid_state() {
        use ducktape_view_guest::wire;
        let (app, _) = PaletteView::boot();
        let mut envelope = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        assert_eq!(envelope.schema, PaletteView::SNAPSHOT_SCHEMA);
        envelope.schema = "0".repeat(64);
        assert!(PaletteView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = PaletteView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(PaletteView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = PaletteView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod snapshot_schema {
    use super::PaletteView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<PaletteView>(PaletteView::SNAPSHOT_SCHEMA, |_| {});
    }
}
