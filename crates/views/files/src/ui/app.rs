use ducktape_view_guest::{kit as native, wire};
#[derive(Default)]
struct DerivedCache {
    refusal: ::std::cell::OnceCell<String>,
    loading: ::std::cell::OnceCell<bool>,
    draft_here: ::std::cell::OnceCell<bool>,
    draft_parked: ::std::cell::OnceCell<bool>,
    edit_context: ::std::cell::OnceCell<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FilesView {
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    pub(crate) chain: String,
    pub(crate) generation: i64,
    pub(crate) route_serial: i64,
    pub(crate) path: String,
    pub(crate) listed: bool,
    pub(crate) entries: Vec<crate::host::FsEntry>,
    pub(crate) directories: Vec<crate::host::FsEntry>,
    pub(crate) history: Vec<crate::host::FsSnapshot>,
    pub(crate) omitted: i64,
    pub(crate) diff_omitted: i64,
    pub(crate) preview_path: String,
    pub(crate) preview_entry: crate::host::FsEntry,
    pub(crate) preview_base: String,
    pub(crate) preview_text: String,
    pub(crate) preview_display_text: String,
    pub(crate) preview_clipped: bool,
    pub(crate) preview_truncated: bool,
    pub(crate) preview_binary: bool,
    pub(crate) preview_picture: bool,
    pub(crate) preview_width: i64,
    pub(crate) preview_height: i64,
    pub(crate) delete_target: String,
    pub(crate) diff_from: String,
    pub(crate) diff: Vec<crate::host::FsDiffEntry>,
    pub(crate) acting: bool,
    pub(crate) saving: bool,
    pub(crate) notice: String,
    pub(crate) new_name: String,
    #[serde(with = "draft_snapshot")]
    pub(crate) draft: ::ducktape_view_guest::Editor,
    pub(crate) editing: bool,
    pub(crate) draft_chain: String,
    pub(crate) draft_path: String,
    pub(crate) draft_base: String,
    pub(crate) draft_id: i64,
    pub(crate) sent: bool,
    pub(crate) viewport_width: f64,
    pub(crate) viewport_height: f64,
    pub(crate) tree_width: f64,
    pub(crate) preview_pane_height: f64,
    pub(crate) object_width: f64,
    #[serde(skip)]
    derived: DerivedCache,
    pub(crate) history_open: bool,
}
impl ::std::fmt::Debug for FilesView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("FilesView")
    }
}
#[derive(Clone)]
pub enum Message {
    TreeResized(f64, f64),
    PreviewResized(f64, f64),
    ObjectResized(f64, f64),
    ViewportChanged(f64, f64),
    SessionArrived(crate::host::SessionItem),
    RouteTo(String),
    ListingArrived(crate::host::ListingItem),
    PreviewArrived(crate::host::PreviewItem),
    DiffArrived(crate::host::DiffItem),
    ActDone(crate::host::ActItem),
    OpenDirAt(String),
    OpenFileAt(String),
    MkdirSubmit,
    NewFileSubmit,
    ArmDeleteAt(String),
    DisarmDeleteNow,
    DeleteSubmit,
    CloseDiffNow,
    ShowDiffOf(String),
    BeginEdit(String),
    CancelEdit(String),
    DiscardDraft(i64),
    SaveEdit(String),
    OpenLinkAt(String),
    ToggleHistory,
    NewNameChanged(String),
    EditDraft(::ducktape_view_guest::EditorDocumentUpdate),
    DraftTransaction(::ducktape_view_guest::EditorTransaction<Message>),
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
            self.tree_width,
            self.preview_pane_height,
            self.object_width,
        ];
        if dimensions.into_iter().all(f64::is_finite) {
            Ok(())
        } else {
            Err("snapshot number must be finite".into())
        }
    }

    #[must_use]
    fn derived_refusal(&self) -> &String {
        self.derived
            .refusal
            .get_or_init(|| crate::host::write_refusal(::std::convert::AsRef::as_ref(&(self.path))))
    }
    fn derived_loading(&self) -> &bool {
        self.derived
            .loading
            .get_or_init(|| (self.acting || self.saving) || (self.connected && (!self.listed)))
    }
    fn derived_draft_here(&self) -> &bool {
        self.derived.draft_here.get_or_init(|| {
            (self.editing && (self.draft_path == self.preview_path))
                && (self.draft_chain == self.chain)
        })
    }
    fn derived_draft_parked(&self) -> &bool {
        self.derived
            .draft_parked
            .get_or_init(|| self.editing && (!(*self.derived_draft_here())))
    }
    fn derived_edit_context(&self) -> &String {
        self.derived.edit_context.get_or_init(|| {
            crate::host::edit_token(
                ::std::convert::AsRef::as_ref(&(self.chain)),
                ::std::convert::AsRef::as_ref(&(self.preview_path)),
                ::std::convert::AsRef::as_ref(&(self.preview_base)),
                self.draft_id,
            )
        })
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
            chain: "".to_owned(),
            generation: 0,
            route_serial: 0,
            path: "/shared".to_owned(),
            listed: false,
            entries: Vec::new(),
            directories: Vec::new(),
            history: Vec::new(),
            omitted: 0,
            diff_omitted: 0,
            preview_path: "".to_owned(),
            preview_entry: crate::host::no_fs_entry(),
            preview_base: "".to_owned(),
            preview_text: "".to_owned(),
            preview_display_text: "".to_owned(),
            preview_clipped: false,
            preview_truncated: false,
            preview_binary: false,
            preview_picture: false,
            preview_width: 0,
            preview_height: 0,
            delete_target: "".to_owned(),
            diff_from: "".to_owned(),
            diff: Vec::new(),
            acting: false,
            saving: false,
            notice: "".to_owned(),
            new_name: "".to_owned(),
            draft: ::ducktape_view_guest::Editor::new("".to_owned()),
            editing: false,
            draft_chain: "".to_owned(),
            draft_path: "".to_owned(),
            draft_base: "".to_owned(),
            draft_id: 0,
            sent: false,
            viewport_width: 1280.0,
            viewport_height: 700.0,
            tree_width: 240.0,
            preview_pane_height: 300.0,
            object_width: 306.0,
            derived: ::std::default::Default::default(),
            history_open: false,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "524542bd8f5b55e48d5274087dd4997a644784746056385b50157aadfe239bba";
}
impl FilesView {
    pub(crate) fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::listing(
                    self.generation,
                    self.path.to_owned(),
                )
                .map(Message::ListingArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected && (!(self.preview_path).is_empty()) {
                ::ducktape_view_guest::Subscription::batch([crate::host::preview(
                    self.generation,
                    self.preview_path.to_owned(),
                )
                .map(Message::PreviewArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected && (!(self.diff_from).is_empty()) {
                ::ducktape_view_guest::Subscription::batch([crate::host::diff(
                    self.generation,
                    self.diff_from.to_owned(),
                )
                .map(Message::DiffArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            crate::host::acts().map(Message::ActDone),
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_preserves_unsaved_document_and_browser_state() {
        let (mut app, _) = FilesView::boot();
        app.history_open = true;
        app.chain = "chain-a".into();
        app.editing = true;
        app.draft_chain = app.chain.clone();
        app.draft_path = "/shared/draft.md".into();
        app.draft_base = "base".into();
        app.draft_id = 42;
        app.draft = ducktape_view_guest::Editor::new("unsaved 한글\nsecond line");
        app.tree_width = 245.;
        app.object_width = 355.;
        app.preview_pane_height = 288.;
        let bytes = app.snapshot().unwrap();
        let restored = FilesView::restore(&bytes).unwrap();
        assert_eq!(restored.snapshot().unwrap(), bytes);
        assert_eq!(restored.draft.text(), "unsaved 한글\nsecond line");
        assert!(restored.history_open);
        assert_eq!(restored.draft_path, "/shared/draft.md");
        assert_eq!(
            (
                restored.tree_width,
                restored.object_width,
                restored.preview_pane_height
            ),
            (245., 355., 288.)
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
    fn restored_state_recomputes_transient_derived_values() {
        let (mut app, _) = FilesView::boot();
        assert!(!app.derived_loading());
        app.acting = true;
        let restored = FilesView::restore(&app.snapshot().unwrap()).unwrap();
        assert!(*restored.derived_loading());
    }
    #[test]
    fn snapshot_rejects_nonfinite_dimensions_on_both_sides() {
        let (mut app, _) = FilesView::boot();
        app.object_width = f64::NAN;
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
        app.listed = true;
        app.chain = "chain-a".into();
        app.preview_path = "/shared/a.md".into();
        app.preview_base = "base-a".into();
        app.editing = true;
        app.draft_chain = app.chain.clone();
        app.draft_path = app.preview_path.clone();
        app.draft_base = app.preview_base.clone();
        app.draft = ducktape_view_guest::Editor::new("unsaved A — 한글");
        let save = Message::SaveEdit(app.derived_edit_context().clone());
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
        assert!(!app.saving);
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
mod browser;
mod files;
mod kit;
