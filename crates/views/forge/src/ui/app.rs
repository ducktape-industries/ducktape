use ducktape_view_guest::{kit as native, wire};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Act {
    Review,
    Merge,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ForgeView {
    #[serde(skip)]
    pub(crate) upload_handles: std::collections::HashMap<String, ducktape_view_guest::task::Handle>,
    pub(crate) composers: std::collections::BTreeMap<String, ducktape_view_composer::Draft>,
    pub(crate) composer_choices: Vec<ducktape_view_composer::MentionChoice>,
    pub(crate) connected: bool,
    pub(crate) dark: bool,
    pub(crate) org: String,
    pub(crate) about: String,
    pub(crate) tier: String,
    pub(crate) network_chain_id: String,
    pub(crate) connected_rpc: String,
    pub(crate) connection_serial: i64,
    pub(crate) link_tick: i64,
    pub(crate) repos: Vec<crate::host::ForgeRepo>,
    pub(crate) list_phase: String,
    pub(crate) open_repo: String,
    pub(crate) repo_phase: String,
    pub(crate) branches: Vec<crate::host::ForgeBranch>,
    pub(crate) items: Vec<crate::host::ForgeItem>,
    pub(crate) tab: String,
    pub(crate) forge_item_number: i64,
    pub(crate) item_phase: String,
    pub(crate) forge_item_kind: String,
    pub(crate) forge_item_title: String,
    pub(crate) forge_item_state: String,
    pub(crate) forge_item_author: String,
    pub(crate) forge_item_branches: String,
    pub(crate) forge_item_body: String,
    pub(crate) forge_item_blocks: Vec<crate::host::ChatBlock>,
    pub(crate) forge_item_files_changed: i64,
    pub(crate) forge_item_additions: i64,
    pub(crate) forge_item_deletions: i64,
    pub(crate) diff_rows: Vec<crate::host::DiffLine>,
    pub(crate) forge_item_diff_truncated: bool,
    pub(crate) forge_item_merge_oid: String,
    pub(crate) forge_item_source_branch: String,
    pub(crate) forge_item_source_oid: String,
    pub(crate) forge_item_target_oid: String,
    pub(crate) forge_item_channel: String,
    pub(crate) forge_item_reviews: Vec<crate::host::ForgeReview>,
    pub(crate) forge_item_approvals: i64,
    pub(crate) forge_item_change_requests: i64,
    pub(crate) discussion: Vec<crate::host::ChatMessage>,
    pub(crate) discussion_clipped: bool,
    pub(crate) linked_note: Vec<crate::host::ChatMessage>,
    pub(crate) focus_seq: i64,
    pub(crate) landed_tick: i64,
    pub(crate) focus_number: i64,
    pub(crate) merge_conflicts: Vec<String>,
    pub(crate) merge_busy: bool,
    pub(crate) review_verdict: String,
    pub(crate) review_busy: bool,
    pub(crate) staged_comments: Vec<crate::host::ForgeDraftComment>,
    pub(crate) tree_pick: String,
    pub(crate) tree_path: String,
    pub(crate) tree_rev: String,
    pub(crate) tree_entries: Vec<crate::host::TreeEntry>,
    /// Every directory listing read at `tree_rev`, by path (`""` is the
    /// root); the viewer paints the tree out of these.
    pub(crate) tree_children: std::collections::BTreeMap<String, Vec<crate::host::TreeEntry>>,
    /// The directories unfolded in the viewer, in the order they were
    /// opened; the root is always unfolded.
    pub(crate) tree_open: Vec<String>,
    pub(crate) tree_born: bool,
    pub(crate) tree_truncated: bool,
    pub(crate) tree_phase: String,
    pub(crate) file_path: String,
    pub(crate) file_text: String,
    pub(crate) file_binary: bool,
    pub(crate) file_truncated: bool,
    pub(crate) file_picture: bool,
    pub(crate) file_width: i64,
    pub(crate) file_height: i64,
    pub(crate) file_note: String,
    pub(crate) file_phase: String,
    pub(crate) opened_dir: String,
    pub(crate) opened_rev: String,
    pub(crate) focus_path: String,
    pub(crate) focus_rev: String,
    pub(crate) review_draft: String,
    pub(crate) comment_draft: String,
    pub(crate) comment_path: String,
    pub(crate) comment_line: String,
    pub(crate) comment_side: String,
    pub(crate) host_error: String,
    pub(crate) sent: bool,
    pub(crate) viewport_width: f64,
    pub(crate) tree_width: f64,
}
impl ::std::fmt::Debug for ForgeView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("ForgeView")
    }
}
#[derive(Clone)]
pub enum Message {
    Composer(Box<composer::ComposerMessage>),
    SessionArrived(crate::host::SessionItem),
    ForgeLandLink(String),
    ReposArrived(crate::host::RepoListItem),
    RepoArrived(crate::host::RepoItem),
    ItemArrived(crate::host::ItemItem),
    DiscussionArrived(crate::host::DiscussionItem),
    TreeArrived(crate::host::TreeItem),
    BlobArrived(crate::host::BlobItem),
    ActDone(crate::host::ActItem),
    ForgeOpenRepo(String),
    ForgeCloseRepo,
    ForgePickBranch(String),
    ForgeOpenDir(String),
    ForgeOpenFile(String),
    ForgeOpenItem(i64),
    ForgeCloseItem,
    SelectForgeTab(String),
    ForgeReviewPick(String),
    ForgeReviewSubmit(String),
    ForgeMergeSubmit,
    ForgeCommentOpen(String, String, String),
    ForgeCommentCancel,
    ForgeCommentStage(String),
    ForgeCommentDrop(String),
    OpenMessageLink(String),
    CopyToClipboard(String, String),
    TreeResized(f64, f64),
    ViewportChanged(f64, f64),
    CommentDraftChanged(String),
    ReviewDraftChanged(String),
    Ignore,
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl ForgeView {
    fn state() -> Self {
        Self {
            composers: Default::default(),
            upload_handles: Default::default(),
            composer_choices: Vec::new(),
            connected: false,
            dark: false,
            org: "".to_owned(),
            about: "".to_owned(),
            tier: "".to_owned(),
            network_chain_id: "".to_owned(),
            connected_rpc: "".to_owned(),
            connection_serial: 0,
            link_tick: 0,
            repos: Vec::new(),
            list_phase: "idle".to_owned(),
            open_repo: "".to_owned(),
            repo_phase: "idle".to_owned(),
            branches: Vec::new(),
            items: Vec::new(),
            tab: "code".to_owned(),
            forge_item_number: 0,
            item_phase: "idle".to_owned(),
            forge_item_kind: "".to_owned(),
            forge_item_title: "".to_owned(),
            forge_item_state: "".to_owned(),
            forge_item_author: "".to_owned(),
            forge_item_branches: "".to_owned(),
            forge_item_body: "".to_owned(),
            forge_item_blocks: Vec::new(),
            forge_item_files_changed: 0,
            forge_item_additions: 0,
            forge_item_deletions: 0,
            diff_rows: Vec::new(),
            forge_item_diff_truncated: false,
            forge_item_merge_oid: "".to_owned(),
            forge_item_source_branch: "".to_owned(),
            forge_item_source_oid: "".to_owned(),
            forge_item_target_oid: "".to_owned(),
            forge_item_channel: "".to_owned(),
            forge_item_reviews: Vec::new(),
            forge_item_approvals: 0,
            forge_item_change_requests: 0,
            discussion: Vec::new(),
            discussion_clipped: false,
            linked_note: Vec::new(),
            focus_seq: 0,
            landed_tick: 0,
            focus_number: 0,
            merge_conflicts: Vec::new(),
            merge_busy: false,
            review_verdict: "comment".to_owned(),
            review_busy: false,
            staged_comments: Vec::new(),
            tree_pick: "".to_owned(),
            tree_path: "".to_owned(),
            tree_rev: "".to_owned(),
            tree_entries: Vec::new(),
            tree_children: Default::default(),
            tree_open: Vec::new(),
            tree_born: false,
            tree_truncated: false,
            tree_phase: "loading".to_owned(),
            file_path: "".to_owned(),
            file_text: "".to_owned(),
            file_binary: false,
            file_truncated: false,
            file_picture: false,
            file_width: 0,
            file_height: 0,
            file_note: "".to_owned(),
            file_phase: "idle".to_owned(),
            opened_dir: "".to_owned(),
            opened_rev: "".to_owned(),
            focus_path: "".to_owned(),
            focus_rev: "".to_owned(),
            review_draft: "".to_owned(),
            comment_draft: "".to_owned(),
            comment_path: "".to_owned(),
            comment_line: "".to_owned(),
            comment_side: "".to_owned(),
            host_error: "".to_owned(),
            sent: false,
            viewport_width: 1280.0,
            tree_width: 260.0,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    /// This state's layout, digested — `snapshot_schema` holds it here.
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "be8e28001029e6cd978e107fc261810b87c570d695269c271c7263553f28ab1c";
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
            return Err("invalid Forge snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Forge snapshot".into());
        };
        let mut state: Self = wire::decode(&state)?;
        for draft in state.composers.values_mut() {
            draft.retire_device_requests();
        }
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let widths = [self.viewport_width, self.tree_width];
        if widths.into_iter().all(f64::is_finite) {
            Ok(())
        } else {
            Err("invalid Forge snapshot geometry".into())
        }
    }
}
impl ForgeView {
    pub(crate) fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::repos(
                    self.connection_serial,
                )
                .map(Message::ReposArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::repo(
                    self.connection_serial,
                    self.open_repo.to_owned(),
                )
                .map(Message::RepoArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::item(
                    self.connection_serial,
                    self.open_repo.to_owned(),
                    self.forge_item_number,
                )
                .map(Message::ItemArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::discussion(
                    self.connection_serial,
                    self.forge_item_channel.to_owned(),
                )
                .map(Message::DiscussionArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::tree(
                    self.connection_serial,
                    self.open_repo.to_owned(),
                    self.tree_rev.to_owned(),
                    self.tree_path.to_owned(),
                )
                .map(Message::TreeArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::blob(
                    self.connection_serial,
                    self.open_repo.to_owned(),
                    self.tree_rev.to_owned(),
                    self.file_path.to_owned(),
                    self.network_chain_id.to_owned(),
                )
                .map(Message::BlobArrived)])
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
    fn snapshot_refuses_foreign_schema_corrupt_payload_and_nonfinite_geometry() {
        let (mut state, _) = ForgeView::boot();
        let mut envelope = wire::Snapshot::decode(&state.snapshot().unwrap()).unwrap();
        envelope.schema = "0".repeat(64);
        assert!(ForgeView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = ForgeView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(ForgeView::restore(&envelope.encode().unwrap()).is_err());
        state.tree_width = f64::INFINITY;
        assert!(state.snapshot().is_err());
        envelope.state = wire::SnapshotValue::Bytes(wire::encode(&state));
        assert!(ForgeView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn view_fits_default_stack() {
        let (app, _) = ForgeView::boot();
        let _ = app.view();
    }

    #[test]
    fn disconnected_view_hides_retained_repository_and_review_controls() {
        let (mut app, _) = ForgeView::boot();
        app.repos.push(crate::host::ForgeRepo {
            name: "stale-repo".into(),
            head: "stale-head".into(),
        });
        app.open_repo = "stale-repo".into();
        app.forge_item_number = 7;
        app.item_phase = "ready".into();
        app.forge_item_kind = "pr".into();
        let mut tree = app.view();
        tree.for_each_mut(&mut |node| {
            assert!(!matches!(
                node,
                wire::Node::Button {
                    on_press: Some(_),
                    ..
                } | wire::Node::Surface { .. }
            ));
            if let wire::Node::Text { content, .. } = node {
                assert!(!content.contains("stale-"));
            }
        });
        assert_eq!(app.repos.len(), 1);
    }

    #[test]
    fn snapshot_preserves_review_drafts_and_code_selection() {
        let (mut app, _) = ForgeView::boot();
        app.open_repo = "core".into();
        app.review_draft = "한글 review".into();
        app.tree_width = 310.;
        app.file_path = "src/main.rs".into();
        app.file_text = "fn main() {}".into();
        app.discussion.push(crate::host::ChatMessage {
            seq: 1,
            author: "reader".into(),
            initial: "R".into(),
            avatar_kind: "human".into(),
            ..Default::default()
        });
        app.linked_note = app.discussion.clone();
        app.staged_comments.push(crate::host::ForgeDraftComment {
            anchor: "src/main.rs:1".into(),
            path: "src/main.rs".into(),
            line: "1".into(),
            side: "new".into(),
            body: "keep".into(),
        });
        let snapshot = app.snapshot().unwrap();
        let restored = ForgeView::restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot);
    }
}
mod app_update;
mod app_view;
mod components;
mod composer;
mod forge;
mod kit;

#[cfg(test)]
mod snapshot_schema {
    use super::ForgeView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<ForgeView>(ForgeView::SNAPSHOT_SCHEMA, |shape| {
            // A draft carries an editor document, which refuses to restore
            // from a byte the tracer made up, so the draft is described by a
            // value that reaches every field it holds.
            shape.sample(&ducktape_view_composer::Draft::schema_sample());
        });
    }
}
