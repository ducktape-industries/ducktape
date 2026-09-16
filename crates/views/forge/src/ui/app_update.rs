use super::*;
impl super::ForgeView {
    pub(crate) fn update(&mut self, message: Message) -> ducktape_view_guest::Task<Message> {
        match message {
            Message::Composer(message) => self.on_composer(*message),
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::SeatArrived(item) => self.on_seat_arrived(item),
            Message::ForgeLandLink(url) => self.on_forge_land_link(url),
            Message::ReposArrived(next) => self.on_repos_arrived(next),
            Message::RepoArrived(next) => self.on_repo_arrived(next),
            Message::ItemArrived(next) => self.on_item_arrived(next),
            Message::DiscussionArrived(next) => self.on_discussion_arrived(next),
            Message::TreeArrived(next) => self.on_tree_arrived(next),
            Message::BlobArrived(next) => self.on_blob_arrived(next),
            Message::ActDone(next) => self.on_act_done(next),
            Message::ForgeOpenRepo(name) => self.on_forge_open_repo(name),
            Message::ForgeCloseRepo => self.on_forge_close_repo(),
            Message::ForgePickBranch(name) => self.on_forge_pick_branch(name),
            Message::ForgeOpenDir(path) => self.on_forge_open_dir(path),
            Message::ForgeRevealDir(path) => self.on_forge_reveal_dir(path),
            Message::ForgeOpenFile(path) => self.on_forge_open_file(path),
            Message::ForgeOpenItem(number) => self.on_forge_open_item(number),
            Message::ForgeCloseItem => self.on_forge_close_item(),
            Message::SelectForgeTab(next) => self.on_select_forge_tab(next),
            Message::SelectTrackerSide(side) => self.on_select_tracker_side(side),
            Message::TrackerFilterChanged(text) => self.on_tracker_filter_changed(text),
            Message::Key(event, captured) => self.on_key(event, captured),
            Message::IssueTitleChanged(value) => self.on_issue_title_changed(value),
            Message::IssueBodyChanged(value) => self.on_issue_body_changed(value),
            Message::ForgeIssueOpen => self.on_forge_issue_open(),
            Message::ForgeReviewPick(verdict) => self.on_forge_review_pick(verdict),
            Message::ForgeReviewSubmit(body) => self.on_forge_review_submit(body),
            Message::ForgeMergeSubmit => self.on_forge_merge_submit(),
            Message::ForgeFoldFile(name) => self.on_forge_fold_file(name),
            Message::ForgeCommentOpen(path, line, side) => {
                self.on_forge_comment_open(path, line, side)
            }
            Message::ForgeCommentCancel => self.on_forge_comment_cancel(),
            Message::ForgeCommentStage(body) => self.on_forge_comment_stage(body),
            Message::ForgeCommentDrop(anchor) => self.on_forge_comment_drop(anchor),
            Message::OpenMessageLink(url) => self.on_open_message_link(url),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::TreeResized(dx, _dy) => self.on_tree_resized(dx, _dy),
            Message::ViewportChanged(width, _height) => self.on_viewport_changed(width, _height),
            Message::CommentDraftChanged(value) => self.on_comment_draft_changed(value),
            Message::ReviewDraftChanged(value) => self.on_review_draft_changed(value),
            Message::Ignore => self.on_ignore(),
        }
    }
    fn on_session_arrived(
        &mut self,
        item: crate::host::SessionItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let next = item.next.clone();
        let connection_changed = self.connected_rpc != next.connected_rpc || self.connected != next.connected;
        if connection_changed {
            self.upload_handles.clear();
            for draft in self.composers.values_mut() { draft.retire_device_requests(); }
        }

        self.connection_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.connection_serial,
        );
        self.connected = next.connected;
        self.dark = next.dark;
        self.org = next.org.to_owned();
        self.about = next.about.to_owned();
        self.network_chain_id = next.network_chain_id.to_owned();
        self.connected_rpc = next.connected_rpc.to_owned();
        let routed = next.link_tick != self.link_tick;
        self.link_tick = next.link_tick;
        ducktape_view_guest::Task::done(Message::ForgeLandLink(crate::host::routed_link(
            routed, &next.link,
        )))
    }

    fn on_seat_arrived(
        &mut self,
        item: crate::host::SeatItem,
    ) -> ducktape_view_guest::Task<Message> {
        let refused = !(item.error).is_empty();
        if refused {
            self.host_error = item.error;
            return ::ducktape_view_guest::Task::none();
        }
        self.tier = item.tier;
        ::ducktape_view_guest::Task::none()
    }

    fn on_forge_land_link(&mut self, url: String) -> ducktape_view_guest::Task<Message> {
        if (url).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let link = crate::host::forge_link(::std::convert::AsRef::as_ref(&(url)));
        if (link.repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.focus_number = link.number;
        self.focus_seq = link.seq;
        self.focus_path = link.path.to_owned();
        self.focus_rev = link.rev.to_owned();
        (::ducktape_view_guest::Task::done(link.repo.to_owned())).map(Message::ForgeOpenRepo)
    }
    fn on_repos_arrived(
        &mut self,
        next: crate::host::RepoListItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = next.error.to_owned();
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.repos = next.repos.clone();
        self.list_phase = "ready".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_repo_arrived(
        &mut self,
        next: crate::host::RepoItem,
    ) -> ducktape_view_guest::Task<Message> {
        if next.repo != self.open_repo {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = next.error.to_owned();
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.branches = next.branches.clone();
        self.items = next.items.clone();
        self.repo_phase = "ready".to_owned();
        let parked = (self.focus_number > 0) || (!(self.focus_path).is_empty());
        if !parked {
            return ::ducktape_view_guest::Task::none();
        }
        self.tree_rev = crate::host::keep_focus(
            self.focus_rev.to_owned(),
            ::std::convert::AsRef::as_ref(&(self.tree_rev)),
        );
        self.tree_path = crate::host::keep_focus(
            crate::host::forge_parent(::std::convert::AsRef::as_ref(&(self.focus_path))),
            ::std::convert::AsRef::as_ref(&(self.tree_path)),
        );
        self.tree_entries = Vec::new();
        self.tree_children.clear();
        self.tree_open.clear();
        self.tree_truncated = false;
        self.tree_phase = "loading".to_owned();
        self.focus_rev = "".to_owned();
        if self.focus_number <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        let number = self.focus_number;
        self.focus_number = 0;
        (::ducktape_view_guest::Task::done(number)).map(Message::ForgeOpenItem)
    }
    fn on_item_arrived(
        &mut self,
        next: crate::host::ItemItem,
    ) -> ducktape_view_guest::Task<Message> {
        if (next.repo != self.open_repo) || (next.number != self.forge_item_number) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = next.error.to_owned();
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let branch_moved = crate::host::forge_branch_moved(
            ::std::convert::AsRef::as_ref(&(next.source_oid)),
            ::std::convert::AsRef::as_ref(&(self.forge_item_source_oid)),
        );
        self.host_error = crate::host::staged_comment_drop_note(
            branch_moved && (!(self.staged_comments).is_empty()),
        );
        self.staged_comments =
            crate::host::keep_staged(branch_moved, ::std::mem::take(&mut self.staged_comments));
        self.comment_draft = crate::host::keep_draft(
            branch_moved,
            ::std::convert::AsRef::as_ref(&(self.comment_draft)),
        );
        self.comment_path = crate::host::keep_draft(
            branch_moved,
            ::std::convert::AsRef::as_ref(&(self.comment_path)),
        );
        self.comment_line = crate::host::keep_draft(
            branch_moved,
            ::std::convert::AsRef::as_ref(&(self.comment_line)),
        );
        self.comment_side = crate::host::keep_draft(
            branch_moved,
            ::std::convert::AsRef::as_ref(&(self.comment_side)),
        );
        self.item_phase = "ready".to_owned();
        self.forge_item_kind = next.kind.to_owned();
        self.tab = crate::host::kind_tab(::std::convert::AsRef::as_ref(&(next.kind)));
        self.forge_item_title = next.title.to_owned();
        self.forge_item_state = next.state.to_owned();
        self.forge_item_author = next.author.to_owned();
        self.forge_item_branches = next.branches.to_owned();
        self.forge_item_body = next.body.to_owned();
        self.forge_item_blocks = next.blocks.clone();
        self.forge_item_channel = next.channel_id.to_owned();
        self.forge_item_source_branch = next.source_branch.to_owned();
        self.forge_item_source_oid = next.source_oid.to_owned();
        self.forge_item_target_oid = next.target_oid.to_owned();
        self.forge_item_merge_oid = next.merge_oid.to_owned();
        self.diff_rows = next.diff_rows.clone();
        self.forge_item_diff_truncated = next.diff_truncated;
        self.forge_item_files_changed = next.files_changed;
        self.forge_item_additions = next.additions;
        self.forge_item_deletions = next.deletions;
        self.forge_item_reviews = next.reviews.clone();
        self.forge_item_approvals = next.approvals;
        self.forge_item_change_requests = next.change_requests;
        ::ducktape_view_guest::Task::none()
    }
    fn on_discussion_arrived(
        &mut self,
        next: crate::host::DiscussionItem,
    ) -> ducktape_view_guest::Task<Message> {
        if next.channel_id != self.forge_item_channel {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = next.error.to_owned();
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.discussion = next.messages.clone();
        self.discussion_clipped = next.clipped;
        self.composer_choices = next.choices.clone();
        if self.focus_seq == 0 {
            return ::ducktape_view_guest::Task::none();
        }
        let landed = self.focus_seq;
        self.focus_seq = 0;
        self.linked_note =
            crate::host::note_at_seq(::std::convert::AsRef::as_ref(&(next.messages)), landed);
        self.landed_tick += 1;
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::ScrollToKey {
                target: String::from("ForgeView/forge/item-detail"),
                key: ::ducktape_view_guest::wire::ListKey::from(landed).virtual_key(),
            },
        )
    }
    fn on_tree_arrived(
        &mut self,
        next: crate::host::TreeItem,
    ) -> ducktape_view_guest::Task<Message> {
        // the listing of no repository (the reader's idle answer) names
        // nothing: taking it would consume a parked deep link and open its
        // file at no commit, which the tree's real answer then hides
        let idle = next.repo.is_empty();
        if idle || (next.repo != self.open_repo) || (next.path != self.tree_path) {
            return ::ducktape_view_guest::Task::none();
        }
        if (!(self.tree_rev).is_empty()) && (next.rev != self.tree_rev) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = next.error.to_owned();
        self.tree_phase = crate::host::phase_of(::std::convert::AsRef::as_ref(&(next.error)));
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.tree_rev = next.rev.to_owned();
        self.tree_born = next.born;
        self.tree_entries = next.entries.clone();
        self.tree_truncated = next.truncated;
        self.tree_children
            .insert(next.path.clone(), next.entries.clone());
        self.unfold_to(&next.path);
        self.request_missing_dir();
        let parked = self.focus_path.to_owned();
        self.focus_path = "".to_owned();
        // a deep link owns the reader; otherwise the repository opens on
        // what it says about itself
        let opening = match parked.is_empty() {
            false => parked,
            true => self.landing_readme(&next),
        };
        if (opening).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        (::ducktape_view_guest::Task::done(opening)).map(Message::ForgeOpenFile)
    }
    /// The README a freshly opened repository lands on: only off the root
    /// listing, and never over a file the reader already holds.
    fn landing_readme(&self, next: &crate::host::TreeItem) -> String {
        let root = next.path.is_empty();
        let reader_is_empty = self.file_path.is_empty();
        if !(root && reader_is_empty) {
            return String::new();
        }
        crate::host::readme_of(&next.entries)
    }
    fn on_blob_arrived(
        &mut self,
        next: crate::host::BlobItem,
    ) -> ducktape_view_guest::Task<Message> {
        let idle = next.repo.is_empty();
        if idle || (next.repo != self.open_repo) || (next.path != self.file_path) {
            return ::ducktape_view_guest::Task::none();
        }
        self.file_note = crate::host::blob_note(&next.note, &next.error);
        self.file_phase = crate::host::phase_of(::std::convert::AsRef::as_ref(&(next.error)));
        if !(next.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.file_text = next.text.to_owned();
        self.file_binary = next.binary;
        self.file_truncated = next.truncated;
        self.file_picture = next.picture;
        self.file_width = next.width;
        self.file_height = next.height;
        ::ducktape_view_guest::Task::none()
    }
    fn on_act_done(&mut self, next: crate::host::ActItem) -> ducktape_view_guest::Task<Message> {
        self.host_error = next.error.to_owned();
        match crate::host::act_of(::std::convert::AsRef::as_ref(&(next.kind))) {
            Act::Issue => {
                self.issue_busy = false;
                if (next.error).is_empty() {
                    self.issue_title = "".to_owned();
                    self.issue_body = "".to_owned();
                }
                ::ducktape_view_guest::Task::none()
            }
            Act::Review => (|| {
                self.review_busy = false;
                if !(next.error).is_empty() {
                    return ::ducktape_view_guest::Task::none();
                }
                self.review_verdict = "comment".to_owned();
                self.staged_comments = Vec::new();
                self.review_draft = "".to_owned();
                self.comment_draft = "".to_owned();
                self.comment_path = "".to_owned();
                self.comment_line = "".to_owned();
                self.comment_side = "".to_owned();
                ::ducktape_view_guest::Task::none()
            })(),
            Act::Merge => {
                self.merge_busy = false;
                self.merge_conflicts = next.conflicts.clone();
                ::ducktape_view_guest::Task::none()
            }
        }
    }
    fn on_forge_open_repo(&mut self, name: String) -> ducktape_view_guest::Task<Message> {
        if !self.connected {
            return ::ducktape_view_guest::Task::none();
        }
        self.open_repo = name.to_owned();
        self.host_error = "".to_owned();
        self.repo_phase = "loading".to_owned();
        self.branches = Vec::new();
        self.items = Vec::new();
        self.tab = "code".to_owned();
        self.item_side = "open".to_owned();
        self.item_filter = "".to_owned();
        self.tree_pick = "".to_owned();
        self.forge_item_number = 0;
        self.item_phase = "idle".to_owned();
        self.forge_item_channel = "".to_owned();
        self.linked_note = Vec::new();
        self.diff_rows = Vec::new();
        self.diff_folded = Vec::new();
        self.discussion = Vec::new();
        self.discussion_clipped = false;
        self.merge_conflicts = Vec::new();
        self.staged_comments = Vec::new();
        self.tree_path = "".to_owned();
        self.tree_rev = "".to_owned();
        self.tree_entries = Vec::new();
        self.tree_children.clear();
        self.tree_open.clear();
        self.tree_born = false;
        self.tree_truncated = false;
        self.tree_phase = "loading".to_owned();
        self.file_path = "".to_owned();
        self.file_text.clear();
        self.file_note = "".to_owned();
        self.file_phase = "idle".to_owned();
        self.opened_dir = "".to_owned();
        self.opened_rev = "".to_owned();
        self.take_the_keyboard()
    }
    fn on_forge_close_repo(&mut self) -> ducktape_view_guest::Task<Message> {
        self.open_repo = "".to_owned();
        self.repo_phase = "idle".to_owned();
        self.branches = Vec::new();
        self.items = Vec::new();
        self.tree_pick = "".to_owned();
        self.forge_item_number = 0;
        self.item_phase = "idle".to_owned();
        self.forge_item_channel = "".to_owned();
        self.linked_note = Vec::new();
        self.focus_seq = 0;
        self.diff_rows = Vec::new();
        self.diff_folded = Vec::new();
        self.discussion = Vec::new();
        self.discussion_clipped = false;
        self.merge_conflicts = Vec::new();
        self.staged_comments = Vec::new();
        self.tree_path = "".to_owned();
        self.tree_rev = "".to_owned();
        self.tree_entries = Vec::new();
        self.tree_children.clear();
        self.tree_open.clear();
        self.tree_born = false;
        self.tree_truncated = false;
        self.tree_phase = "loading".to_owned();
        self.file_path = "".to_owned();
        self.file_text.clear();
        self.file_note = "".to_owned();
        self.file_phase = "idle".to_owned();
        self.opened_dir = "".to_owned();
        self.opened_rev = "".to_owned();
        self.take_the_keyboard()
    }
    fn on_forge_pick_branch(&mut self, name: String) -> ducktape_view_guest::Task<Message> {
        if (!self.connected) || (self.open_repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let head = crate::host::forge_branch_head(
            ::std::convert::AsRef::as_ref(&(self.branches)),
            ::std::convert::AsRef::as_ref(&(name)),
        );
        if (head).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.tree_pick = name.to_owned();
        self.tree_path = "".to_owned();
        self.tree_entries = Vec::new();
        self.tree_children.clear();
        self.tree_open.clear();
        self.tree_truncated = false;
        self.tree_phase = "loading".to_owned();
        self.tree_rev = head.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_open_dir(&mut self, path: String) -> ducktape_view_guest::Task<Message> {
        if (!self.connected) || (self.open_repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        // a fold takes the directories under it along, so it reopens folded
        let folded = !self.tree_open.contains(&path);
        match folded {
            true => self.tree_open.push(path),
            false => self
                .tree_open
                .retain(|open| open != &path && !open.starts_with(&format!("{path}/"))),
        }
        self.request_missing_dir();
        ::ducktape_view_guest::Task::none()
    }
    /// Unfold `dir` and every directory above it, so a listing that arrived
    /// for a deep link is on screen.
    fn unfold_to(&mut self, dir: &str) {
        let mut prefix = String::new();
        for segment in dir.split('/').filter(|segment| !segment.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if !self.tree_open.contains(&prefix) {
                self.tree_open.push(prefix.clone());
            }
        }
    }
    /// Ask the host for the first unfolded directory not read yet, the root
    /// before any other; one listing travels at a time.
    fn request_missing_dir(&mut self) {
        let missing = std::iter::once("")
            .chain(self.tree_open.iter().map(String::as_str))
            .find(|dir| !self.tree_children.contains_key(*dir))
            .map(str::to_owned);
        let Some(dir) = missing else {
            return;
        };
        let already_asked = dir == self.tree_path && self.tree_phase == "loading";
        if already_asked {
            return;
        }
        self.tree_path = dir;
        self.tree_entries = Vec::new();
        self.tree_truncated = false;
        self.tree_phase = "loading".to_owned();
    }
    /// A breadcrumb names a directory to SHOW, not to toggle: pressing the
    /// crumb of a directory already unfolded must leave it unfolded, which
    /// is exactly what `ForgeOpenDir` would not do.
    fn on_forge_reveal_dir(&mut self, path: String) -> ducktape_view_guest::Task<Message> {
        if (!self.connected) || (self.open_repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.unfold_to(&path);
        self.request_missing_dir();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_open_file(&mut self, path: String) -> ducktape_view_guest::Task<Message> {
        if (!self.connected) || (self.open_repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.opened_dir = crate::host::forge_parent(&path);
        self.opened_rev = self.tree_rev.to_owned();
        self.file_path = path.to_owned();
        self.file_text.clear();
        self.file_binary = false;
        self.file_truncated = false;
        self.file_picture = false;
        self.file_note = "".to_owned();
        self.file_phase = "loading".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_open_item(&mut self, number: i64) -> ducktape_view_guest::Task<Message> {
        if (!self.connected) || (self.open_repo).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.forge_item_number = number;
        self.linked_note = Vec::new();
        self.host_error = "".to_owned();
        self.item_phase = "loading".to_owned();
        self.forge_item_channel = "".to_owned();
        self.review_verdict = "comment".to_owned();
        self.staged_comments = Vec::new();
        self.review_draft = "".to_owned();
        self.comment_draft = "".to_owned();
        self.comment_path = "".to_owned();
        self.comment_line = "".to_owned();
        self.comment_side = "".to_owned();
        self.merge_conflicts = Vec::new();
        self.diff_rows = Vec::new();
        self.diff_folded = Vec::new();
        self.discussion = Vec::new();
        self.discussion_clipped = false;
        self.take_the_keyboard()
    }
    fn on_forge_close_item(&mut self) -> ducktape_view_guest::Task<Message> {
        self.forge_item_number = 0;
        self.item_phase = "idle".to_owned();
        self.forge_item_channel = "".to_owned();
        self.linked_note = Vec::new();
        self.focus_seq = 0;
        self.diff_rows = Vec::new();
        self.diff_folded = Vec::new();
        self.discussion = Vec::new();
        self.discussion_clipped = false;
        self.merge_conflicts = Vec::new();
        self.staged_comments = Vec::new();
        self.take_the_keyboard()
    }
    fn on_select_forge_tab(&mut self, next: String) -> ducktape_view_guest::Task<Message> {
        self.tab = next.to_owned();
        if self.forge_item_number <= 0 {
            return self.take_the_keyboard();
        }
        // the close carries its own focus request; this one is for the tab
        // press itself, so the keys land on the page either way
        (::ducktape_view_guest::Task::done(true)).map(|_value| Message::ForgeCloseItem)
    }
    fn on_select_tracker_side(&mut self, side: String) -> ducktape_view_guest::Task<Message> {
        self.item_side = side;
        self.take_the_keyboard()
    }
    fn on_tracker_filter_changed(&mut self, text: String) -> ducktape_view_guest::Task<Message> {
        self.item_filter = text;
        ::ducktape_view_guest::Task::none()
    }
    /// The window's keys. A native field that took the key owns it, so
    /// typing a slash into the filter does not re-focus it.
    fn on_key(
        &mut self,
        event: ducktape_view_guest::wire::keyboard::Event,
        captured: bool,
    ) -> ducktape_view_guest::Task<Message> {
        use ducktape_view_guest::wire::keyboard::{Event, Key};
        let Event::Press { state, .. } = event else {
            return ::ducktape_view_guest::Task::none();
        };
        let typing_or_modified = captured || state.modifiers.alt || state.modifiers.control;
        if typing_or_modified {
            return ::ducktape_view_guest::Task::none();
        }
        let pressed = match &state.key {
            Key::Character(text) => text.to_owned(),
            Key::Named(named) => format!("{named:?}"),
            _ => return ::ducktape_view_guest::Task::none(),
        };
        match pressed.as_str() {
            "Escape" => self.on_walk_back(),
            "/" => self.on_reach_for_the_filter(),
            _ => ::ducktape_view_guest::Task::none(),
        }
    }
    /// Escape leaves whatever is open, one step at a time: a started issue,
    /// then the item, then the repository, and it does nothing at the
    /// namespace. The draft goes first because it is the only step that
    /// throws work away — leaving the repository under a half-written issue
    /// would take it with no way back.
    fn on_walk_back(&mut self) -> ducktape_view_guest::Task<Message> {
        let drafting = !self.issue_title.trim().is_empty() || !self.issue_body.trim().is_empty();
        if drafting {
            self.issue_title.clear();
            self.issue_body.clear();
            return ::ducktape_view_guest::Task::none();
        }
        let item_open = self.forge_item_number > 0;
        if item_open {
            return self.on_forge_close_item();
        }
        let repo_open = !self.open_repo.is_empty();
        if repo_open {
            return self.on_forge_close_repo();
        }
        ::ducktape_view_guest::Task::none()
    }
    /// The window's keys reach a view only while something inside it holds
    /// the focus, so every press that MOVES the forge screen takes it back
    /// to the page. Nothing takes it after that, which leaves a field the
    /// reader clicked into holding the keyboard while they type.
    fn take_the_keyboard(&self) -> ducktape_view_guest::Task<Message> {
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: crate::PAGE_KEY.to_owned(),
            },
        )
    }
    /// `/` puts the keyboard in the tracker filter, where a tracker is what
    /// is on screen.
    fn on_reach_for_the_filter(&mut self) -> ducktape_view_guest::Task<Message> {
        let tracker_on_screen =
            self.connected && !self.open_repo.is_empty() && self.forge_item_number == 0;
        let tracker_tab = self.tab == "issues" || self.tab == "pulls";
        if !(tracker_on_screen && tracker_tab) {
            return ::ducktape_view_guest::Task::none();
        }
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: crate::TRACKER_FILTER_KEY.to_owned(),
            },
        )
    }
    fn on_issue_title_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.issue_title = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_issue_body_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.issue_body = value;
        ::ducktape_view_guest::Task::none()
    }
    /// The module refuses an empty title, so the composer never sends one;
    /// every other refusal (the repo's open-item cap, the author's share of
    /// it, a body over the wire's cap) comes back on the act.
    fn on_forge_issue_open(&mut self) -> ducktape_view_guest::Task<Message> {
        if !self.can_open_issue() {
            return ::ducktape_view_guest::Task::none();
        }
        self.issue_busy = true;
        self.sent = crate::host::issue_open(
            self.open_repo.to_owned(),
            self.issue_title.to_owned(),
            self.issue_body.to_owned(),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_review_pick(&mut self, verdict: String) -> ducktape_view_guest::Task<Message> {
        self.review_verdict = verdict.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_review_submit(&mut self, body: String) -> ducktape_view_guest::Task<Message> {
        if (self.review_busy || (!self.connected)) || (self.forge_item_source_oid).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        if (body).is_empty() && (self.staged_comments).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.review_busy = true;
        self.sent = crate::host::review_submit(
            self.open_repo.to_owned(),
            self.forge_item_number,
            self.review_verdict.to_owned(),
            body.to_owned(),
            self.forge_item_source_oid.to_owned(),
            self.staged_comments.clone(),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_merge_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (((!self.connected) || self.merge_busy) || (self.open_repo).is_empty())
            || (self.forge_item_number <= 0)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.merge_busy = true;
        self.merge_conflicts = Vec::new();
        self.sent = crate::host::merge(
            self.open_repo.to_owned(),
            self.forge_item_number,
            self.forge_item_source_branch.to_owned(),
            self.forge_item_source_oid.to_owned(),
            self.forge_item_target_oid.to_owned(),
        );
        ::ducktape_view_guest::Task::none()
    }
    /// Put one changed file away, or bring it back.
    fn on_forge_fold_file(&mut self, name: String) -> ducktape_view_guest::Task<Message> {
        let folded = self.diff_folded.iter().any(|held| held == &name);
        match folded {
            true => self.diff_folded.retain(|held| held != &name),
            false => self.diff_folded.push(name),
        }
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_comment_open(
        &mut self,
        path: String,
        line: String,
        side: String,
    ) -> ducktape_view_guest::Task<Message> {
        if (path).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.comment_path = path.to_owned();
        self.comment_line = line.to_owned();
        self.comment_side = side.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_comment_cancel(&mut self) -> ducktape_view_guest::Task<Message> {
        self.comment_path = "".to_owned();
        self.comment_line = "".to_owned();
        self.comment_side = "".to_owned();
        self.comment_draft = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_comment_stage(&mut self, body: String) -> ducktape_view_guest::Task<Message> {
        if ((self.comment_path).is_empty() || (body).is_empty())
            || crate::host::forge_comment_cap_reached(::std::convert::AsRef::as_ref(
                &(self.staged_comments),
            ))
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.staged_comments = crate::host::stage_forge_comment(
            ::std::mem::take(&mut self.staged_comments),
            self.comment_path.to_owned(),
            self.comment_line.to_owned(),
            self.comment_side.to_owned(),
            body.to_owned(),
        );
        self.comment_path = "".to_owned();
        self.comment_line = "".to_owned();
        self.comment_side = "".to_owned();
        self.comment_draft = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_forge_comment_drop(&mut self, anchor: String) -> ducktape_view_guest::Task<Message> {
        self.staged_comments = crate::host::drop_forge_comment(
            ::std::mem::take(&mut self.staged_comments),
            ::std::convert::AsRef::as_ref(&(anchor)),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_message_link(&mut self, url: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::open_link(::std::convert::AsRef::as_ref(&(url)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_copy_to_clipboard(
        &mut self,
        text: String,
        label: String,
    ) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::copy(
            ::std::convert::AsRef::as_ref(&(text)),
            ::std::convert::AsRef::as_ref(&(label)),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_tree_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.tree_width =
            crate::host::tree_width_after_delta(self.tree_width, dx, self.viewport_width);
        ::ducktape_view_guest::Task::none()
    }
    fn on_viewport_changed(
        &mut self,
        width: f64,
        _height: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.viewport_width = width;
        self.tree_width = crate::host::tree_width_after_delta(self.tree_width, 0.0, width);
        ::ducktape_view_guest::Task::none()
    }
    fn on_comment_draft_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.comment_draft = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_review_draft_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.review_draft = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_ignore(&mut self) -> ducktape_view_guest::Task<Message> {
        ::ducktape_view_guest::Task::none()
    }
}
