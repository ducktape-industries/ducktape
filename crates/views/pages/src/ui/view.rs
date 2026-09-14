use ducktape_view_guest::{Subscription, Task, wire};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CommentsMode {
    Beside,
    Squeeze,
    Inline,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PagesView {
    pub(crate) pointer_y: f64,
    pub(crate) comment_anchor_y: f64,
    pub(crate) comment_anchor_line: i64,
    /// The exact text a comment started from the format menu anchors on, in
    /// the module's UTF-16 units over the block; `None` is the whole block.
    pub(crate) comment_anchor_range: Option<(u32, u32)>,
    /// The agent account the open composer's comment is addressed to — an
    /// "Ask AI" pick — or `0` for a plain comment.
    pub(crate) comment_mention: i64,
    pub(crate) comments_card_height: f64,
    /// The network's named members: what an `@` in the document completes to.
    pub(crate) member_names: Vec<String>,
    /// The active agents "Ask AI" can address: display name and account.
    pub(crate) member_agents: Vec<(String, u64)>,
    /// The one comment being rewritten in place, and its words.
    pub(crate) comment_edit_id: String,
    pub(crate) comment_edit_draft: String,
    pub(crate) document_reserve: crate::editor_view::EditorReserve,
    pub(crate) document_paint: crate::editor_view::PreparedPresentation,
    #[serde(with = "document_snapshot")]
    pub(crate) document: ducktape_view_guest::Editor,
    pub(crate) document_history: crate::editor_binding::HistoryState,
    pub(crate) document_menu: crate::editor_binding::MenuState,
    pub(crate) document_error: String,
    pub(crate) document_dark: bool,
    pub(crate) document_commented: Vec<i64>,
    pub(crate) document_marks: Vec<crate::document_sync::CommentMark>,
    pub(crate) connected: bool,
    pub(crate) chain: String,
    pub(crate) route_serial: i64,
    pub(crate) register_serial: i64,
    pub(crate) loading: bool,
    pub(crate) busy: bool,
    pub(crate) host_error: String,
    pub(crate) page_link: String,
    pub(crate) pages: Vec<crate::host::PageItem>,
    /// Parents whose subtree the sidebar keeps folded shut.
    pub(crate) folded_pages: Vec<String>,
    pub(crate) blocks: Vec<crate::document_sync::PageBlock>,
    pub(crate) pages_viewport_width: f64,
    pub(crate) pages_viewport_height: f64,
    pub(crate) pages_pane_width: f64,
    pub(crate) sidebar_width: f64,
    pub(crate) page_menu_open: bool,
    /// The sidebar row whose "…" menu is open; empty when none is.
    pub(crate) page_menu_page: String,
    /// The page the delete dialog asks about; empty when it is closed.
    pub(crate) page_delete_page: String,
    /// Where the last press landed on the view, so a menu opens there.
    pub(crate) press_x: f64,
    pub(crate) press_y: f64,
    pub(crate) page_create_open: bool,
    pub(crate) active_page: String,
    pub(crate) active_page_title: String,
    pub(crate) active_page_parent: String,
    pub(crate) page_searching: bool,
    pub(crate) page_search_hits: Vec<crate::host::PageSearchHit>,
    pub(crate) page_search_query: String,
    pub(crate) page_search_serial: i64,
    pub(crate) page_delete_armed: bool,
    pub(crate) autosave: String,
    pub(crate) page_refusal: String,
    pub(crate) subpages: Vec<crate::host::Subpage>,
    pub(crate) orphaned_comment_drafts: Vec<String>,
    pub(crate) block_comments_open: bool,
    pub(crate) scope_target: String,
    pub(crate) scope_pinned: bool,
    pub(crate) thread_total: i64,
    pub(crate) comment_rows: Vec<crate::host::PageCommentThreadRow>,
    pub(crate) threads_loading: bool,
    pub(crate) commented_hits: Vec<String>,
    pub(crate) reply_thread: String,
    pub(crate) expanded_threads: Vec<String>,
    pub(crate) resolved_open: bool,
    pub(crate) page_draft: String,
    pub(crate) page_search_draft: String,
    pub(crate) block_comment_draft: String,
    pub(crate) reply_draft: String,
    pub(crate) pending_page: String,
    pub(crate) pending_comment: String,
    pub(crate) page_saved_text: String,
    pub(crate) buffer_page: String,
    pub(crate) page_inflight_text: String,
}
impl ::std::fmt::Debug for PagesView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PagesView")
    }
}
#[derive(Clone)]
pub enum Message {
    SessionArrived(crate::host::SessionItem),
    CommentPointerMoved(f64, f64),
    ChoosePage(String),
    TogglePageFold(String),
    RegisterArrived(crate::host::RegisterItem),
    SearchArrived(crate::host::SearchItem),
    ActDone(crate::host::ActItem),
    SaveDone(crate::host::SaveItem),
    PageAutosaveTick,
    TogglePageCreate,
    CreatePageSubmit,
    ArmPageDelete(String),
    DisarmPageDelete,
    AddSubpage(String),
    OpenPageRowMenu(String),
    PressedAt(f64, f64),
    DeletePageSubmit,
    SearchPagesSubmit,
    ClearPageSearch,
    OpenPageSearchHit(String, String),
    UseOrphanedCommentDraft(String),
    DiscardOrphanedCommentDraft(String),
    ToggleBlockComments,
    CloseBlockComments,
    NarrowCommentScope(String),
    WidenCommentScope,
    ResolveThreadSubmit(String, bool),
    SelectReplyThread(String),
    ToggleThreadReplies(String),
    ToggleResolvedComments,
    PostThreadReply(String),
    PostBlockCommentSubmit,
    BeginEditComment(String, String),
    CancelEditComment,
    SubmitEditComment,
    DeleteCommentSubmit(String),
    CommentEditDraftChanged(String),
    CopyToClipboard(String, String),
    SidebarResized(f64, f64),
    PagesViewportChanged(f64, f64),
    PagesPaneResized(f64, f64),
    CommentsCardMeasured(f64, f64),
    TogglePageMenu,
    ClosePageMenu,
    DocumentCommitted(crate::editor_binding::EditorUpdate),
    PageDraftChanged(String),
    SearchDraftChanged(String),
    ReplyDraftChanged(String),
    CommentDraftChanged(String),
    DocumentUpdated(::ducktape_view_guest::EditorDocumentUpdate),
    DocumentTransaction(::ducktape_view_guest::EditorTransaction<Message>),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl PagesView {
    fn initial_state() -> Self {
        Self {
            pointer_y: 0.0,
            comment_anchor_y: -1.0,
            comment_anchor_line: 0,
            comment_anchor_range: None,
            comment_mention: 0,
            comments_card_height: 0.0,
            member_names: Vec::new(),
            member_agents: Vec::new(),
            comment_edit_id: "".to_owned(),
            comment_edit_draft: "".to_owned(),
            document_reserve: crate::editor_view::no_reserve(),
            document_paint: crate::editor_view::empty_presentation(),
            document: ::ducktape_view_guest::Editor::new("".to_owned()),
            document_history: crate::editor_binding::initial_history(),
            document_menu: crate::editor_binding::initial_menu(),
            document_error: "".to_owned(),
            document_dark: false,
            document_commented: Vec::new(),
            document_marks: Vec::new(),
            connected: false,
            chain: "".to_owned(),
            route_serial: 0,
            register_serial: 0,
            loading: false,
            busy: false,
            host_error: "".to_owned(),
            page_link: "".to_owned(),
            pages: Vec::new(),
            folded_pages: Vec::new(),
            blocks: Vec::new(),
            pages_viewport_width: 1280.0,
            pages_viewport_height: 700.0,
            pages_pane_width: 1280.0,
            sidebar_width: 230.0,
            page_menu_open: false,
            page_menu_page: "".to_owned(),
            page_delete_page: "".to_owned(),
            press_x: 0.,
            press_y: 0.,
            page_create_open: false,
            active_page: "".to_owned(),
            active_page_title: "".to_owned(),
            active_page_parent: "".to_owned(),
            page_searching: false,
            page_search_hits: Vec::new(),
            page_search_query: "".to_owned(),
            page_search_serial: 0,
            page_delete_armed: false,
            autosave: "idle".to_owned(),
            page_refusal: "".to_owned(),
            subpages: Vec::new(),
            orphaned_comment_drafts: Vec::new(),
            block_comments_open: false,
            scope_target: "".to_owned(),
            scope_pinned: false,
            thread_total: 0,
            comment_rows: Vec::new(),
            threads_loading: false,
            commented_hits: Vec::new(),
            reply_thread: "".to_owned(),
            expanded_threads: Vec::new(),
            resolved_open: false,
            page_draft: "".to_owned(),
            page_search_draft: "".to_owned(),
            block_comment_draft: "".to_owned(),
            reply_draft: "".to_owned(),
            pending_page: "".to_owned(),
            pending_comment: "".to_owned(),
            page_saved_text: "".to_owned(),
            buffer_page: "".to_owned(),
            page_inflight_text: "".to_owned(),
        }
    }
    pub(crate) fn boot() -> (Self, Task<Message>) {
        (Self::initial_state(), Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    const SNAPSHOT_SCHEMA: &'static str =
        "ddab2698f12a601ef232bf2cb53ee7cb4618866c31a238b9f25208c4ea70af56";
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
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Pages snapshot".into());
        };
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid Pages snapshot schema".into());
        }
        let state: Self = wire::decode(&state)?;
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let finite_layout = [
            self.pointer_y,
            self.comment_anchor_y,
            self.comments_card_height,
            self.pages_viewport_width,
            self.pages_viewport_height,
            self.pages_pane_width,
            self.sidebar_width,
        ]
        .into_iter()
        .all(f64::is_finite);
        if !finite_layout {
            return Err("invalid Pages layout snapshot".into());
        }
        self.document_paint.validate(&self.document)
    }
}

mod document_snapshot {
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
            .ok_or_else(|| serde::de::Error::custom("invalid document snapshot"))
    }
}

impl PagesView {
    fn subscription(&self) -> Subscription<Message> {
        let search_active = self.connected && !self.page_search_query.is_empty();
        let autosave_ready = self.connected
            && !self.loading
            && !self.busy
            && !self.active_page.is_empty()
            && self.active_page == self.buffer_page;
        Subscription::batch([
            ::ducktape_view_guest::mouse::observe(Subscription::filter_events(
                |event| match event {
                    wire::Event::Mouse {
                        event: wire::mouse::Event::CursorMoved { x, y },
                        ..
                    } => Some(Message::CommentPointerMoved(*x as f64, *y as f64)),
                    _ => None,
                },
            )),
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                crate::host::register(self.active_page.to_owned(), self.register_serial)
                    .map(Message::RegisterArrived)
            } else {
                Subscription::none()
            },
            if search_active {
                crate::host::search(self.page_search_query.to_owned(), self.page_search_serial)
                    .map(Message::SearchArrived)
            } else {
                Subscription::none()
            },
            crate::host::acts().map(Message::ActDone),
            crate::host::saves().map(Message::SaveDone),
            if autosave_ready {
                ducktape_view_guest::every(::std::time::Duration::from_millis(900))
                    .map(|_| Message::PageAutosaveTick)
            } else {
                Subscription::none()
            },
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_reject_nonfinite_layout_in_every_coordinate() {
        let fields: [fn(&mut PagesView) -> &mut f64; 7] = [
            |s| &mut s.pointer_y,
            |s| &mut s.comment_anchor_y,
            |s| &mut s.comments_card_height,
            |s| &mut s.pages_viewport_width,
            |s| &mut s.pages_viewport_height,
            |s| &mut s.pages_pane_width,
            |s| &mut s.sidebar_width,
        ];
        for field in fields {
            let mut state = PagesView::initial_state();
            *field(&mut state) = f64::NAN;
            assert!(state.snapshot().is_err());
            let bytes = wire::Snapshot {
                schema: PagesView::SNAPSHOT_SCHEMA.into(),
                state: wire::SnapshotValue::Bytes(wire::encode(&state)),
            }
            .encode()
            .unwrap();
            assert!(PagesView::restore(&bytes).is_err());
        }
    }

    #[test]
    fn snapshots_reject_malformed_cached_presentation() {
        let mut state = PagesView::initial_state();
        state.document_paint.data = vec![0xff];
        assert!(state.snapshot().is_err());
        let bytes = wire::Snapshot {
            schema: PagesView::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(&state)),
        }
        .encode()
        .unwrap();
        assert!(PagesView::restore(&bytes).is_err());
    }
    #[test]
    fn ordinary_state_preserves_document_and_drafts_through_snapshot() {
        let (mut app, _) = PagesView::boot();
        app.document = ducktape_view_guest::Editor::new("# 제목\n\nDocument 🙂");
        app.page_draft = "새 페이지".into();
        app.block_comment_draft = "Unsent comment".into();
        app.reply_draft = "Unsent reply".into();
        app.expanded_threads = vec!["thread-a".into()];
        app.document_history.snapshot = vec![1, 2, 3];
        app.document_menu.snapshot = vec![4, 5];
        app.page_search_draft = "Unsubmitted search".into();
        app.orphaned_comment_drafts = vec!["Recovered draft".into()];
        app.pending_page = "Submitted title".into();
        app.pending_comment = "Submitted comment".into();
        app.page_inflight_text = "Submitted document".into();
        app.page_saved_text = "Acknowledged document".into();
        app.scope_target = "block-a".into();
        app.reply_thread = "thread-a".into();
        app.block_comments_open = true;
        app.pages_pane_width = 1060.;
        let snapshot = app.snapshot().unwrap();
        let restored = PagesView::restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot);
    }
    /// A sidebar row carries "…" and "+": the "+" asks the host for a page
    /// inside and unfolds the parent so the newcomer shows; the "…" (or a
    /// right press) opens the row's own menu at the last press, whose
    /// Delete arms the dialog for THAT page, not the open one.
    #[test]
    fn a_sidebar_row_adds_a_subpage_and_opens_its_own_menu() {
        use ducktape_view_guest::testing::keys;
        let page = |id: &str, parent: &str, depth: usize, children: i64| crate::host::PageItem {
            id: id.into(),
            title: id.to_uppercase(),
            parent: parent.into(),
            prefix: "  ".repeat(depth),
            child_count: children,
        };
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.active_page = "alpha".into();
        app.pages = vec![
            page("alpha", "", 0, 1),
            page("alpha-child", "alpha", 1, 0),
            page("beta", "", 0, 0),
        ];
        app.update(Message::TogglePageFold("alpha".into()));
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        let present = keys(&frame);
        let has = |present: &[String], key: &str| present.iter().any(|k| k == key);
        assert!(has(&present, "PagesView/root/pages/page/beta/add"));
        assert!(has(&present, "PagesView/root/pages/page/beta/more"));
        assert!(has(&present, "PagesView/root/pages/page/beta/area"));
        assert!(!has(&present, "PagesView/root/pages/page/beta/menu"));
        assert!(!has(&present, "PagesView/root/pages/menu-backdrop"));

        app.update(Message::AddSubpage("alpha".into()));
        assert!(app.busy, "the host is asked for the page");
        assert!(
            app.folded_pages.is_empty(),
            "the parent unfolds for its newcomer"
        );

        app.busy = false;
        app.update(Message::PressedAt(120., 200.));
        app.update(Message::OpenPageRowMenu("beta".into()));
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        let opened = keys(&frame);
        assert!(has(&opened, "PagesView/root/pages/page/beta/menu"));
        assert!(has(&opened, "PagesView/root/pages/page/beta/menu/delete"));
        assert!(has(&opened, "PagesView/root/pages/menu-backdrop"));

        app.update(Message::ArmPageDelete("beta".into()));
        assert!(app.page_delete_armed);
        assert_eq!(app.page_delete_page, "beta");
        assert_eq!(app.page_menu_page, "", "the menu closes behind the dialog");
        assert_eq!(app.active_page, "alpha", "the open page is not the target");
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        assert!(!has(&keys(&frame), "PagesView/root/pages/page/beta/menu"));

        app.update(Message::DeletePageSubmit);
        assert!(app.busy);
        assert_eq!(app.active_page, "alpha", "deleting a sibling moves nothing");

        app.busy = false;
        app.update(Message::ArmPageDelete("alpha".into()));
        app.update(Message::DeletePageSubmit);
        assert_eq!(
            app.active_page, "",
            "deleting the open root lands on the list"
        );
    }

    /// The sidebar is Notion's page tree: a child sits one step under its
    /// parent, a parent carries a fold toggle, and folding it hides its
    /// whole subtree — and only that.
    #[test]
    fn the_sidebar_folds_a_parents_subtree_and_nothing_else() {
        use ducktape_view_guest::testing::keys;
        let page = |id: &str, parent: &str, depth: usize, children: i64| crate::host::PageItem {
            id: id.into(),
            title: id.to_uppercase(),
            parent: parent.into(),
            prefix: "  ".repeat(depth),
            child_count: children,
        };
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.pages = vec![
            page("alpha", "", 0, 1),
            page("alpha-child", "alpha", 1, 1),
            page("alpha-grandchild", "alpha-child", 2, 0),
            page("beta", "", 0, 0),
        ];
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        let present = keys(&frame);
        assert!(
            present
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha/fold")
        );
        assert!(
            present
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha-grandchild")
        );
        assert!(
            !present
                .iter()
                .any(|key| key == "PagesView/root/pages/page/beta/fold"),
            "a leaf has no toggle"
        );

        app.update(Message::TogglePageFold("alpha".into()));
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        let folded = keys(&frame);
        assert!(
            folded
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha")
        );
        assert!(
            !folded
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha-child")
        );
        assert!(
            !folded
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha-grandchild")
        );
        assert!(
            folded
                .iter()
                .any(|key| key == "PagesView/root/pages/page/beta")
        );

        app.update(Message::TogglePageFold("alpha".into()));
        let frame = wire::Frame {
            root: Some(app.view()),
            ..Default::default()
        };
        assert!(
            keys(&frame)
                .iter()
                .any(|key| key == "PagesView/root/pages/page/alpha-child")
        );
    }
    #[test]
    fn kit_composition_retains_editor_and_comment_routes_without_custom_control_faces() {
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.active_page = "page-a".into();
        app.buffer_page = app.active_page.clone();
        app.page_create_open = true;
        app.block_comments_open = true;
        app.comment_rows = vec![crate::host::PageCommentThreadRow {
            thread: crate::host::PageCommentThread {
                id: "thread-a".into(),
                ..Default::default()
            },
            ..Default::default()
        }];
        app.reply_thread = "thread-a".into();
        let mut editor = false;
        let mut inputs = Vec::new();
        app.view().for_each_mut(&mut |node| match node {
            Node::Editor {
                key,
                editable,
                options,
                ..
            } => {
                assert_eq!(key, &format!("{PAGE_KEY}/document"));
                assert!(*editable);
                assert!(options.binding.is_some());
                assert!(options.presentation.is_some());
                assert_eq!(options.style, wire::InputStyle::default());
                editor = true;
            }
            Node::Input {
                key,
                options,
                style,
                ..
            } => {
                assert!(!options.disabled);
                assert_eq!(**style, wire::InputStyle::default());
                inputs.push(key.clone());
            }
            Node::Button { style, .. } => {
                let preset = style.preset;
                assert_eq!(
                    *style,
                    wire::ButtonStyle {
                        preset,
                        ..Default::default()
                    }
                );
            }
            _ => {}
        });
        assert!(editor);
        assert!(inputs.contains(&format!("{PAGE_KEY}/new-page")));
        assert!(inputs.contains(&format!("{PAGE_KEY}/page-search")));
        assert!(inputs.contains(&format!("{PAGE_KEY}/page-comment(page-a)")));
        assert!(inputs.contains(&format!("{PAGE_KEY}/thread-reply(thread-a)")));
        app.loading = true;
        let Node::Editor { editable, .. } = app.document_editor() else {
            unreachable!()
        };
        assert!(!editable, "loading must not edit the previous page");
    }
    #[test]
    fn measured_pane_changes_the_guest_document_width() {
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.active_page = "page".into();
        app.block_comments_open = true;
        let _ = app.update(Message::PagesPaneResized(1060., 700.));
        assert_eq!(app.pages_pane_width, 1060.);
        let mut constrained = false;
        app.view().for_each_mut(&mut |node| {
            if let wire::Node::Container {
                max_width: Some(width),
                ..
            } = node
            {
                constrained |= *width == 688.;
            }
        });
        assert!(
            constrained,
            "the guest must publish the measured squeeze width"
        );
    }
    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = PagesView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn disconnected_composition_does_not_expose_stale_pages_search_or_counts() {
        let (mut app, _) = PagesView::boot();
        app.pages = vec![crate::host::PageItem {
            id: "stale-page".into(),
            title: "Stale page title".into(),
            ..Default::default()
        }];
        app.page_search_draft = "stale".into();
        app.page_search_query = "stale".into();
        app.page_search_hits = vec![crate::host::PageSearchHit {
            page_title: "Stale search title".into(),
            ..Default::default()
        }];
        let mut content = Vec::new();
        let mut keys = Vec::new();
        app.view().for_each_mut(&mut |node| {
            if let Some(key) = node.key() {
                keys.push(key.to_string());
            }
            if let Node::Text { content: text, .. } = node {
                content.push(text.clone());
            }
        });
        assert!(content.contains(&"Not connected".into()));
        assert!(
            !keys
                .iter()
                .any(|key| key == "pages/sidebar/count" || key == "pages/search/results")
        );
        assert!(!content.iter().any(|text| text.starts_with("Stale")));
    }

    #[test]
    fn empty_search_without_selected_page_keeps_clear_action_on_native_overlay() {
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.page_search_draft = "missing".into();
        app.page_search_query = "missing".into();
        let mut found = false;
        app.view().for_each_mut(&mut |node| {
            let Node::Overlay {
                key,
                children,
                on_dismiss,
                ..
            } = node
            else {
                return;
            };
            if key != "pages/search" {
                return;
            }
            assert!(on_dismiss.is_some());
            assert_eq!(
                children.len(),
                2,
                "native overlay paints its modal surface over the base"
            );
            let mut clear = false;
            children[1].for_each_mut(&mut |child| {
                if let Node::Button {
                    label, on_press, ..
                } = child
                {
                    clear |= label.as_deref() == Some("Clear search") && on_press.is_some();
                }
            });
            assert!(
                clear,
                "no result must still provide an exit without the page toolbar"
            );
            found = true;
        });
        assert!(found);
    }

    #[test]
    fn an_empty_search_answer_never_describes_an_unsubmitted_draft() {
        let (mut app, _) = PagesView::boot();
        app.connected = true;
        app.page_search_query = "missing".into();
        app.page_search_draft = "  missing  ".into();
        let empty_answer = |app: &PagesView| {
            let mut found = false;
            app.view().for_each_mut(&mut |node| {
                if let Node::Text { content, .. } = node {
                    found |= content == "No matching pages";
                }
            });
            found
        };
        assert!(empty_answer(&app));
        app.page_search_draft = "different".into();
        assert!(!empty_answer(&app));
        app.page_search_draft = "missing".into();
        app.page_searching = true;
        assert!(!empty_answer(&app), "pending is not an empty answer");
    }
}
include!("app_update.rs");
include!("app_view.rs");
include!("kit.rs");
include!("pages.rs");
include!("rows.rs");
