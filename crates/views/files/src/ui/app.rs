//! The Files view's state, its messages and its subscriptions: a Finder-style
//! browser over duckfs. The sidebar names the places (shared, the homes,
//! recent snapshots), the toolbar carries Back / Forward / Up, the path bar,
//! the actions and the filter, the main pane lists the directory as rows or
//! as Miller columns, the inspector previews the chosen file and states what
//! is known about it, and the status bar counts what is on screen.

use ducktape_view_guest::{kit as native, wire};

use crate::host::Act;
use crate::host::{FsEntry, FsSnapshot};
use browse::{BrowseKey, Listing, NamePrompt, Navigation, Sort, SortKey, ViewMode};

/// The chosen file's read, as the inspector shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Preview {
    pub path: String,
    pub base: String,
    pub text: String,
    pub display_text: String,
    pub clipped: bool,
    pub truncated: bool,
    pub binary: bool,
    pub picture: bool,
    pub width: i64,
    pub height: i64,
    pub error: String,
}

impl Preview {
    /// A file just chosen: the path is set and nothing has been read yet.
    pub(crate) fn of(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            ..Self::default()
        }
    }

    /// Has the read landed? Until the head snapshot arrives with the page,
    /// nothing has been read and a blank body would read as an empty file.
    pub(crate) fn is_read(&self) -> bool {
        !self.base.is_empty() || self.binary || self.picture || !self.error.is_empty()
    }
}

/// The snapshot that last touched the chosen path, once the walk answers.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Provenance {
    pub path: String,
    pub snapshot: FsSnapshot,
    pub searched: i64,
    pub answered: bool,
    pub error: String,
}

/// A write in flight, or none. One value: the toolbar's wait word and the
/// disabled controls read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Writing {
    Idle,
    Busy(Act),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FilesView {
    // ---- the session ----
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    /// the network a draft belongs to, across reconnections to it
    pub(crate) chain: String,
    /// the reader's account, which names her home
    pub(crate) account: String,
    /// moves when the session comes up and after every write: every read restarts
    pub(crate) generation: i64,
    /// the last `duck://files/...` push this view has landed on
    pub(crate) route_serial: i64,
    // ---- where the reader stands ----
    pub(crate) nav: Navigation,
    pub(crate) listing: Listing,
    /// how many pages of the directory have been asked for
    pub(crate) pages: i64,
    /// the directories open to the right of the current one, in column view
    pub(crate) columns: Vec<String>,
    pub(crate) column_listings: std::collections::BTreeMap<String, Listing>,
    pub(crate) homes: Vec<FsEntry>,
    pub(crate) history: Vec<FsSnapshot>,
    pub(crate) history_error: String,
    // ---- what is chosen ----
    /// the chosen row: a file (then previewed) or a folder (then inspected)
    pub(crate) selected: String,
    pub(crate) preview: Preview,
    pub(crate) provenance: Provenance,
    pub(crate) diff_from: String,
    pub(crate) diff: Vec<crate::host::FsDiffEntry>,
    pub(crate) diff_omitted: i64,
    // ---- how the pane is laid out ----
    pub(crate) view_mode: ViewMode,
    pub(crate) sort: Sort,
    pub(crate) filter: String,
    pub(crate) sidebar_open: bool,
    pub(crate) inspector_open: bool,
    // ---- the writes ----
    pub(crate) name_prompt: NamePrompt,
    pub(crate) name_draft: String,
    pub(crate) delete_target: String,
    pub(crate) writing: Writing,
    /// the last refusal or failure the view has to say
    pub(crate) notice: String,
    // ---- the editor over the previewed file ----
    #[serde(with = "draft_snapshot")]
    pub(crate) draft: ::ducktape_view_guest::Editor,
    pub(crate) editing: bool,
    pub(crate) draft_chain: String,
    pub(crate) draft_path: String,
    pub(crate) draft_base: String,
    pub(crate) draft_id: i64,
    /// a write's acknowledgement — `host::notify` returns nothing to bind
    pub(crate) sent: bool,
    // ---- the geometry ----
    pub(crate) viewport_width: f64,
    pub(crate) viewport_height: f64,
    pub(crate) sidebar_width: f64,
    pub(crate) inspector_width: f64,
    pub(crate) column_widths: Vec<f64>,
    pub(crate) chosen_width: f64,
}

impl ::std::fmt::Debug for FilesView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("FilesView")
    }
}

#[derive(Clone)]
pub enum Message {
    // the readings
    SessionArrived(crate::host::SessionItem),
    RouteTo(String),
    WorkspaceArrived(crate::host::WorkspaceItem),
    PreviewArrived(crate::host::PreviewItem),
    ProvenanceArrived(crate::host::ProvenanceItem),
    DiffArrived(crate::host::DiffItem),
    ActDone(crate::host::ActItem),
    // the navigation
    Navigate(String),
    Back,
    Forward,
    Parent,
    Refresh,
    LoadMore,
    Select(String),
    Open(String),
    KeyPressed(BrowseKey),
    // the pane
    SetViewMode(ViewMode),
    SortBy(SortKey),
    FilterChanged(String),
    ToggleSidebar,
    ToggleInspector,
    // the writes
    Prompt(NamePrompt),
    NameChanged(String),
    NameSubmit,
    ArmDelete(String),
    DisarmDelete,
    DeleteSubmit,
    // the history
    ShowDiffOf(String),
    CloseDiff,
    // the editor
    BeginEdit(String),
    CancelEdit(String),
    DiscardDraft(i64),
    SaveEdit(String),
    EditDraft(::ducktape_view_guest::EditorDocumentUpdate),
    DraftTransaction(::ducktape_view_guest::EditorTransaction<Message>),
    // the app's doors
    OpenLinkAt(String),
    // the geometry
    SidebarResized(f64, f64),
    InspectorResized(f64, f64),
    ColumnResized(usize, f64, f64),
    ChosenResized(f64, f64),
    ViewportChanged(f64, f64),
}

impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}

impl FilesView {
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.validate_snapshot()?;
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid Files snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Files snapshot".into());
        };
        let state: Self = wire::decode(&state)?;
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let dimensions = [
            self.viewport_width,
            self.viewport_height,
            self.sidebar_width,
            self.inspector_width,
            self.chosen_width,
        ];
        let finite = dimensions
            .into_iter()
            .chain(self.column_widths.iter().copied())
            .all(f64::is_finite);
        if finite {
            Ok(())
        } else {
            Err("snapshot number must be finite".into())
        }
    }

    // ---- the readings the screen and the handlers share ----

    /// duckfs's write rule for the current directory, or "".
    pub(crate) fn refusal(&self) -> String {
        crate::host::write_refusal(&self.nav.path)
    }

    /// A write is out, or the directory has not answered yet: the controls
    /// that would race it wait.
    pub(crate) fn loading(&self) -> bool {
        self.writing != Writing::Idle || (self.connected && self.listing.is_pending())
    }

    pub(crate) fn busy_writing(&self) -> bool {
        self.writing != Writing::Idle
    }

    pub(crate) fn draft_here(&self) -> bool {
        self.editing && self.draft_path == self.preview.path && self.draft_chain == self.chain
    }

    pub(crate) fn draft_parked(&self) -> bool {
        self.editing && !self.draft_here()
    }

    pub(crate) fn edit_context(&self) -> String {
        crate::host::edit_token(
            &self.chain,
            &self.preview.path,
            &self.preview.base,
            self.draft_id,
        )
    }

    /// The reader's own home, or "" while the session names no account.
    pub(crate) fn home(&self) -> String {
        crate::host::home_of(&self.account)
    }

    /// The rows the main pane shows for the current directory.
    pub(crate) fn rows(&self) -> Vec<FsEntry> {
        browse::visible_rows(self.listing.entries(), &self.filter, self.sort)
    }

    /// The chosen row, as the listing on hand knows it.
    pub(crate) fn selected_entry(&self) -> FsEntry {
        let in_place = crate::host::entry_named(self.listing.entries(), &self.selected);
        if !in_place.path.is_empty() {
            return in_place;
        }
        self.column_listings
            .values()
            .map(|listing| crate::host::entry_named(listing.entries(), &self.selected))
            .find(|entry| !entry.path.is_empty())
            .unwrap_or_default()
    }
}

mod draft_snapshot {
    use serde::{Deserialize, Serialize};

    pub fn serialize<S: serde::Serializer>(
        editor: &ducktape_view_guest::Editor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        editor.snapshot().serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ducktape_view_guest::Editor, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        ducktape_view_guest::Editor::restore(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid draft snapshot"))
    }
}

impl FilesView {
    fn state() -> Self {
        Self {
            connected: false,
            dark: false,
            chain: String::new(),
            account: String::new(),
            generation: 0,
            route_serial: 0,
            nav: Navigation::at("/shared"),
            listing: Listing::Pending,
            pages: 1,
            columns: Vec::new(),
            column_listings: Default::default(),
            homes: Vec::new(),
            history: Vec::new(),
            history_error: String::new(),
            selected: String::new(),
            preview: Preview::default(),
            provenance: Provenance::default(),
            diff_from: String::new(),
            diff: Vec::new(),
            diff_omitted: 0,
            view_mode: ViewMode::List,
            sort: Sort::BY_NAME,
            filter: String::new(),
            sidebar_open: true,
            inspector_open: true,
            name_prompt: NamePrompt::Closed,
            name_draft: String::new(),
            delete_target: String::new(),
            writing: Writing::Idle,
            notice: String::new(),
            draft: ::ducktape_view_guest::Editor::new(String::new()),
            editing: false,
            draft_chain: String::new(),
            draft_path: String::new(),
            draft_base: String::new(),
            draft_id: 0,
            sent: false,
            viewport_width: 1280.0,
            viewport_height: 700.0,
            sidebar_width: 200.0,
            inspector_width: 340.0,
            column_widths: Vec::new(),
            chosen_width: 290.0,
        }
    }

    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }

    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "7f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
}

impl FilesView {
    pub(crate) fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        use ::ducktape_view_guest::Subscription;
        let connected = self.connected;
        let previewing = connected && !self.preview.path.is_empty();
        let inspecting = connected && !self.selected.is_empty() && !self.history.is_empty();
        let comparing = connected && !self.diff_from.is_empty();
        let columns = match self.view_mode {
            ViewMode::Columns => self.columns.clone(),
            ViewMode::List => Vec::new(),
        };
        Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            gated(connected, || {
                crate::host::workspace(self.generation, self.nav.path.clone(), self.pages, columns)
                    .map(Message::WorkspaceArrived)
            }),
            gated(previewing, || {
                crate::host::preview(self.generation, self.preview.path.clone())
                    .map(Message::PreviewArrived)
            }),
            gated(inspecting, || {
                crate::host::provenance(
                    self.generation,
                    self.selected.clone(),
                    self.history.clone(),
                )
                .map(Message::ProvenanceArrived)
            }),
            gated(comparing, || {
                crate::host::diff(self.generation, self.diff_from.clone()).map(Message::DiffArrived)
            }),
            crate::host::acts().map(Message::ActDone),
            // A key the focused control did not take is the browser's:
            // arrows move the selection, Enter opens it, Backspace and ⌘↑ go
            // up, ⌘← / ⌘→ walk the trail. A press a field consumed never
            // arrives here, so typing a name is never a navigation.
            gated(connected, || {
                Subscription::filter_events(|event| match event {
                    wire::Event::Keyboard {
                        event: wire::keyboard::Event::Press { state, .. },
                        captured,
                    } => {
                        let key = browse::browse_key(state);
                        // Native fields consume Escape, but the enclosing
                        // dialog still owns dismissal.
                        let ignored =
                            key == BrowseKey::Ignored || (*captured && key != BrowseKey::Cancel);
                        if ignored {
                            return None;
                        }
                        Some(Message::KeyPressed(key))
                    }
                    _ => None,
                })
            }),
        ])
    }
}

/// A subscription that exists only while its condition holds.
fn gated<M: 'static>(
    on: bool,
    make: impl FnOnce() -> ::ducktape_view_guest::Subscription<M>,
) -> ::ducktape_view_guest::Subscription<M> {
    match on {
        true => make(),
        false => ::ducktape_view_guest::Subscription::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resizing_and_toggling_rails_preserve_the_filename_column() {
        let (mut app, _) = FilesView::boot();
        for width in [1280., 1080., 800., 760., 620., 590., 580., 490., 480., 400.] {
            drop(app.update(Message::ViewportChanged(width, 680.)));
            for message in [Message::ToggleSidebar, Message::ToggleInspector] {
                drop(app.update(message));
                let sidebar = if app.sidebar_open {
                    app.sidebar_width + 10.
                } else {
                    0.
                };
                let inspector = if app.inspector_open {
                    app.inspector_width + 10.
                } else {
                    0.
                };
                assert!(
                    width - sidebar - inspector >= 320.,
                    "filename space at {width}"
                );
            }
        }
        drop(app.update(Message::ViewportChanged(620., 680.)));
        drop(app.update(Message::ToggleInspector));
        assert!(app.inspector_open);
        drop(app.update(Message::ToggleSidebar));
        assert!(app.sidebar_open);
        assert!(!app.inspector_open);
        drop(app.update(Message::ToggleInspector));
        assert!(app.inspector_open);
        assert!(!app.sidebar_open);
    }

    #[test]
    fn snapshot_preserves_unsaved_document_and_browser_state() {
        let (mut app, _) = FilesView::boot();
        app.chain = "chain-a".into();
        app.editing = true;
        app.draft_chain = app.chain.clone();
        app.draft_path = "/shared/draft.md".into();
        app.draft_base = "base".into();
        app.draft_id = 42;
        app.draft = ducktape_view_guest::Editor::new("unsaved 한글\nsecond line");
        app.nav.go("/shared/docs");
        app.view_mode = ViewMode::Columns;
        app.sort = Sort::BY_NAME.toggled(SortKey::Size);
        app.sidebar_width = 245.;
        app.inspector_width = 355.;
        app.column_widths = vec![280., 360.];
        app.chosen_width = 420.;
        let bytes = app.snapshot().unwrap();
        let restored = FilesView::restore(&bytes).unwrap();
        assert_eq!(restored.snapshot().unwrap(), bytes);
        assert_eq!(restored.column_widths, [280., 360.]);
        assert_eq!(restored.chosen_width, 420.);
        assert_eq!(restored.draft.text(), "unsaved 한글\nsecond line");
        assert_eq!(restored.draft_path, "/shared/draft.md");
        assert_eq!(restored.nav.path, "/shared/docs");
        assert_eq!(restored.nav.back, ["/shared"]);
        assert_eq!(restored.view_mode, ViewMode::Columns);
        assert_eq!(restored.sort.key, SortKey::Size);
        assert_eq!(
            (restored.sidebar_width, restored.inspector_width),
            (245., 355.)
        );
    }

    #[test]
    fn snapshot_rejects_wrong_schema_and_invalid_editor_state() {
        let (app, _) = FilesView::boot();
        let mut snapshot = wire::Snapshot::decode(&app.snapshot().unwrap()).unwrap();
        snapshot.schema = "0".repeat(64);
        assert!(FilesView::restore(&snapshot.encode().unwrap()).is_err());
        let mut value = serde_json::to_value(&app).unwrap();
        value["draft"] = serde_json::json!([255, 255]);
        assert!(serde_json::from_value::<FilesView>(value).is_err());
    }

    #[test]
    fn snapshot_rejects_nonfinite_dimensions_on_both_sides() {
        let (mut app, _) = FilesView::boot();
        app.inspector_width = f64::NAN;
        assert!(app.snapshot().is_err());
        let bytes = wire::Snapshot {
            schema: FilesView::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(&app)),
        }
        .encode()
        .unwrap();
        assert!(FilesView::restore(&bytes).is_err());
    }

    #[test]
    fn a_captured_save_refuses_after_its_network_moves() {
        let (mut app, _) = FilesView::boot();
        app.connected = true;
        app.listing = Listing::Listed {
            entries: Vec::new(),
            next: String::new(),
        };
        app.chain = "chain-a".into();
        app.preview = Preview::of("/shared/a.md");
        app.preview.base = "base-a".into();
        app.editing = true;
        app.draft_chain = app.chain.clone();
        app.draft_path = app.preview.path.clone();
        app.draft_base = app.preview.base.clone();
        app.draft = ducktape_view_guest::Editor::new("unsaved A — 한글");
        let save = Message::SaveEdit(app.edit_context());
        let _ = app.update(Message::SessionArrived(crate::host::SessionItem {
            next: crate::host::Session {
                connected: true,
                chain: "chain-b".into(),
                ..Default::default()
            },
            ..Default::default()
        }));
        // Message IDs belong to a frame; admission tests retain the actual
        // domain command rather than replaying an ID against another table.
        let _ = app.update(save);
        assert!(!app.sent);
        assert_eq!(app.writing, Writing::Idle);
        assert_eq!(app.draft.text(), "unsaved A — 한글");
        assert_eq!(app.draft_chain, "chain-a");
        assert_eq!(app.draft_path, "/shared/a.md");
        assert_eq!(app.draft_base, "base-a");
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = FilesView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

mod app_update;
mod app_view;
pub mod browse;
mod columns;
mod inspector;
mod kit;
mod listing;
mod sidebar;
