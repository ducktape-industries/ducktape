//! Every message the browser answers, one handler per variant. A handler
//! decides with the pure model in `browse.rs` and then writes state; every
//! move of the reader goes through [`FilesView::land`], the one place the
//! per-directory state resets.

use super::*;
use ::ducktape_view_guest::Task;

impl FilesView {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::FilesDropped(files) => self.on_files_dropped(files),
            Message::FilesUploaded(serial, result) => self.on_files_uploaded(serial, result),
            Message::GrantsReleased => self.on_grants_released(),
            Message::RouteTo(target) => self.on_route_to(target),
            Message::WorkspaceArrived(item) => self.on_workspace_arrived(item),
            Message::PreviewArrived(item) => self.on_preview_arrived(item),
            Message::ProvenanceArrived(item) => self.on_provenance_arrived(item),
            Message::DiffArrived(item) => self.on_diff_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::Navigate(target) => self.on_navigate(target),
            Message::Back => self.on_back(),
            Message::Forward => self.on_forward(),
            Message::Parent => self.on_parent(),
            Message::Refresh => self.on_refresh(),
            Message::LoadMore => self.on_load_more(),
            Message::Select(path) => self.on_select(path),
            Message::Open(path) => self.on_open(path),
            Message::KeyPressed(key) => self.on_key_pressed(key),
            Message::SetViewMode(mode) => self.on_set_view_mode(mode),
            Message::SortBy(key) => self.on_sort_by(key),
            Message::FilterChanged(filter) => self.on_filter_changed(filter),
            Message::ToggleSidebar => self.on_toggle_sidebar(),
            Message::ToggleInspector => self.on_toggle_inspector(),
            Message::Prompt(prompt) => self.on_prompt(prompt),
            Message::NameChanged(name) => self.on_name_changed(name),
            Message::NameSubmit => self.on_name_submit(),
            Message::ArmDelete(target) => self.on_arm_delete(target),
            Message::DisarmDelete => self.on_disarm_delete(),
            Message::DeleteSubmit => self.on_delete_submit(),
            Message::ShowDiffOf(id) => self.on_show_diff_of(id),
            Message::CloseDiff => self.on_close_diff(),
            Message::BeginEdit(token) => self.on_begin_edit(token),
            Message::CancelEdit(token) => self.on_cancel_edit(token),
            Message::DiscardDraft(id) => self.on_discard_draft(id),
            Message::SaveEdit(token) => self.on_save_edit(token),
            Message::EditDraft(document) => self.on_edit_draft(document),
            Message::DraftTransaction(transaction) => self.on_draft_transaction(transaction),
            Message::OpenLinkAt(url) => self.on_open_link_at(url),
            Message::SidebarResized(dx, _dy) => self.on_sidebar_resized(dx),
            Message::InspectorResized(dx, _dy) => self.on_inspector_resized(dx),
            Message::ColumnResized(index, dx, _dy) => self.on_column_resized(index, dx),
            Message::ChosenResized(dx, _dy) => self.on_chosen_resized(dx),
            Message::ViewportChanged(width, height) => self.on_viewport_changed(width, height),
        }
    }

    fn on_files_dropped(
        &mut self,
        files: Result<Vec<ducktape_view_files::SelectedFile>, String>,
    ) -> Task<Message> {
        let files = match files {
            Ok(files) => files,
            Err(error) => {
                self.notice = error;
                return Task::none();
            }
        };
        let unavailable = !self.connected || self.busy_writing();
        if unavailable {
            return Task::perform(
                async move {
                    for file in files {
                        ducktape_view_files::release(&file.token).await;
                    }
                },
                |_| Message::GrantsReleased,
            );
        }
        self.upload_serial += 1;
        let serial = self.upload_serial;
        let directory = self.nav.path.clone();
        let (task, handle) =
            Task::perform(crate::host::upload_files(files, directory), move |result| {
                Message::FilesUploaded(serial, result)
            })
            .abortable();
        self.upload = Some(handle.abort_on_drop());
        task
    }

    fn on_grants_released(&mut self) -> Task<Message> {
        Task::none()
    }

    fn on_files_uploaded(&mut self, serial: u64, result: Result<(), String>) -> Task<Message> {
        if serial != self.upload_serial {
            return Task::none();
        }
        self.upload = None;
        self.notice = result.err().unwrap_or_default();
        self.generation += 1;
        Task::none()
    }

    // ---- the one reset ----

    /// The reader has moved: everything that belonged to the directory she
    /// left goes with it, the new directory is asked for, and the window's
    /// drop door is told where the view now stands.
    fn land(&mut self) {
        self.listing = Listing::Pending;
        self.pages = 1;
        self.columns.clear();
        self.column_listings.clear();
        self.filter.clear();
        self.clear_choice();
        self.notice.clear();
        self.name_prompt = NamePrompt::Closed;
        self.name_draft.clear();
    }

    /// Nothing is chosen: no preview, no provenance, no comparison.
    fn clear_choice(&mut self) {
        self.selected.clear();
        self.preview = Preview::default();
        self.provenance = Provenance::default();
    }

    /// The chosen row is `path`: a file is previewed, a folder is inspected.
    /// In column view a chosen folder also opens to the right.
    fn choose(&mut self, entry: FsEntry) {
        self.selected = entry.path.clone();
        self.provenance = Provenance::default();
        self.preview = match entry.is_dir() {
            true => Preview::default(),
            false => Preview::of(&entry.path),
        };
        let in_columns = self.view_mode == ViewMode::Columns;
        match (in_columns, entry.is_dir()) {
            (true, true) => self.open_column(&entry.path),
            (true, false) => self.close_columns_after(&entry.path),
            (false, _) => {}
        }
    }

    /// The columns to the right of the one holding `path` close: a chosen
    /// file ends the chain at its own column.
    fn close_columns_after(&mut self, path: &str) {
        let keep = self.column_owning(path).unwrap_or(0);
        self.columns.truncate(keep);
        self.column_listings
            .retain(|column, _| self.columns.contains(column));
    }

    /// The columns to the right of the current directory end at the parent
    /// of `path` and continue with it.
    fn open_column(&mut self, path: &str) {
        let parent = crate::host::fs_parent(path);
        let keep = self
            .columns
            .iter()
            .position(|column| *column == parent)
            .map_or(0, |index| index + 1);
        self.columns.truncate(keep);
        self.columns.push(path.to_owned());
        self.column_listings
            .retain(|column, _| self.columns.contains(column));
    }

    // ---- the readings ----

    fn on_session_arrived(&mut self, item: crate::host::SessionItem) -> Task<Message> {
        if !item.error.is_empty() {
            self.notice = item.error;
            return Task::none();
        }
        let next = item.next;
        let rebound = self.chain != next.chain || !next.connected;
        if rebound {
            self.upload = None;
            self.upload_serial += 1;
        }
        let came_up = next.connected && !self.connected;
        if came_up {
            self.generation += 1;
        }
        // rows read over a connection that dropped are no longer this path's
        let dropped = !next.connected && self.connected;
        if dropped {
            self.listing = Listing::Pending;
            self.column_listings.clear();
        }
        self.connected = next.connected;
        self.chain = next.chain;
        self.account = next.account;
        self.dark = next.dark;
        // A duck:// LINK LANDS ON THE FILE. The shell resolved the address and
        // moved the tab; the path itself is a session fact, and the SERIAL —
        // not the path — says a push happened, so the same file twice opens
        // twice.
        let routed = next.route_serial != self.route_serial && !next.route.is_empty();
        self.route_serial = next.route_serial;
        match routed {
            true => Task::done(Message::RouteTo(next.route)),
            false => Task::none(),
        }
    }

    /// WHERE THE LINK SENT THE READER. The address names a FILE: its directory
    /// is what the browser lists, the file itself is what the inspector
    /// reads. The same address twice is the same two subscription keys, so
    /// the generation is what makes the second push read again.
    fn on_route_to(&mut self, target: String) -> Task<Message> {
        self.generation += 1;
        self.nav.go(&crate::host::fs_parent(&target));
        self.land();
        self.selected = target.clone();
        self.preview = Preview::of(&target);
        Task::none()
    }

    fn on_workspace_arrived(&mut self, item: crate::host::WorkspaceItem) -> Task<Message> {
        let for_here = item.directory.path == self.nav.path;
        if !for_here {
            return Task::none();
        }
        self.listing = listing_of(item.directory);
        self.column_listings = item
            .columns
            .into_iter()
            .filter(|column| self.columns.contains(&column.path))
            .map(|column| (column.path.clone(), listing_of(column)))
            .collect();
        self.homes = item.homes;
        self.history_error = item.history_error;
        if self.history_error.is_empty() {
            self.history = item.history;
        }
        // a chosen row its directory no longer holds is no longer chosen —
        // judged only against a directory read WHOLE: a refusal or a page
        // not yet walked says nothing about the row
        let owner = self.listing_owning(&self.selected).cloned();
        let complete = matches!(&owner, Some(Listing::Listed { next, .. }) if next.is_empty());
        let gone = complete && self.selected_entry().path.is_empty();
        if gone {
            self.clear_choice();
        }
        Task::none()
    }

    /// The listing of the directory `path` sits in: the current one, or an
    /// open column's; `None` when neither is on screen.
    fn listing_owning(&self, path: &str) -> Option<&Listing> {
        let parent = crate::host::fs_parent(path);
        if parent == self.nav.path {
            return Some(&self.listing);
        }
        self.column_listings.get(&parent)
    }

    /// Which column the row `path` belongs to: 0 for the current directory,
    /// `i + 1` for the column opened for `columns[i]`, `None` for a row no
    /// column shows.
    fn column_owning(&self, path: &str) -> Option<usize> {
        let parent = crate::host::fs_parent(path);
        if parent == self.nav.path {
            return Some(0);
        }
        self.columns
            .iter()
            .position(|column| *column == parent)
            .map(|index| index + 1)
    }

    /// The rows column `index` shows: the current directory takes the
    /// filter and the sort, an open column is name order.
    pub(super) fn rows_of_column(&self, index: usize) -> Vec<FsEntry> {
        if index == 0 {
            return self.rows();
        }
        let Some(path) = self.columns.get(index - 1) else {
            return Vec::new();
        };
        let entries = self
            .column_listings
            .get(path)
            .map(Listing::entries)
            .unwrap_or_default();
        browse::visible_rows(entries, "", Sort::BY_NAME)
    }

    fn on_preview_arrived(&mut self, item: crate::host::PreviewItem) -> Task<Message> {
        if item.path != self.preview.path {
            return Task::none();
        }
        self.preview = Preview {
            path: item.path,
            base: item.base,
            text: item.text,
            display_text: item.display_text,
            clipped: item.clipped,
            truncated: item.truncated,
            binary: item.binary,
            picture: item.picture,
            width: item.width,
            height: item.height,
            error: item.error,
        };
        Task::none()
    }

    fn on_provenance_arrived(&mut self, item: crate::host::ProvenanceItem) -> Task<Message> {
        if item.path != self.selected {
            return Task::none();
        }
        self.provenance = Provenance {
            path: item.path,
            snapshot: item.snapshot,
            searched: item.searched,
            answered: true,
            error: item.error,
        };
        Task::none()
    }

    fn on_diff_arrived(&mut self, item: crate::host::DiffItem) -> Task<Message> {
        if !item.error.is_empty() {
            self.notice = item.error;
            return Task::none();
        }
        self.diff = item.entries;
        self.diff_omitted = item.omitted;
        Task::none()
    }

    /// EVERY WRITE'S OUTCOME IS THIS VIEW'S OWN. A committed write consumes
    /// the prompt it came from, and moves the generation so the directory,
    /// the history and the open preview are read again. A refused write
    /// leaves the prompt or the confirm OPEN with the refusal inside it, so
    /// the reader fixes the name or backs out from where she stands.
    fn on_act_done(&mut self, item: crate::host::ActItem) -> Task<Message> {
        self.notice = item.error.clone();
        self.writing = Writing::Idle;
        let committed = item.error.is_empty();
        if !committed {
            return Task::none();
        }
        match item.act {
            Act::Mkdir | Act::NewFile => self.name_committed(),
            Act::Rename => self.rename_committed(),
            Act::Delete => self.delete_committed(),
            Act::Save => self.save_committed(),
        }
        self.generation += 1;
        Task::none()
    }

    fn name_committed(&mut self) {
        self.name_prompt = NamePrompt::Closed;
        self.name_draft.clear();
    }

    fn rename_committed(&mut self) {
        let NamePrompt::Rename(from) = std::mem::replace(&mut self.name_prompt, NamePrompt::Closed)
        else {
            return;
        };
        let to = crate::host::fs_child(&crate::host::fs_parent(&from), &self.name_draft);
        self.name_draft.clear();
        let chosen = self.selected == from;
        if chosen {
            self.selected = to.clone();
            let previewing = !self.preview.path.is_empty();
            if previewing {
                self.preview = Preview::of(&to);
            }
            self.provenance = Provenance::default();
        }
    }

    /// The deleted object, or anything under it, can no longer be the
    /// chosen row — the preview would show a file that is gone.
    fn delete_committed(&mut self) {
        let target = std::mem::take(&mut self.delete_target);
        let under = format!("{}/", target.trim_end_matches('/'));
        let chosen = self.selected == target || self.selected.starts_with(&under);
        if chosen {
            self.clear_choice();
        }
        let columns_under = self
            .columns
            .iter()
            .any(|column| *column == target || column.starts_with(&under));
        if columns_under {
            self.columns.clear();
            self.column_listings.clear();
        }
    }

    fn save_committed(&mut self) {
        self.editing = false;
    }

    // ---- the navigation ----

    fn on_navigate(&mut self, target: String) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        let moved = self.nav.go(&target);
        if moved {
            self.land();
        }
        Task::none()
    }

    fn on_back(&mut self) -> Task<Message> {
        if self.connected && self.nav.back() {
            self.land();
        }
        Task::none()
    }

    fn on_forward(&mut self) -> Task<Message> {
        if self.connected && self.nav.forward() {
            self.land();
        }
        Task::none()
    }

    fn on_parent(&mut self) -> Task<Message> {
        if self.nav.at_root() {
            return Task::none();
        }
        self.on_navigate(crate::host::fs_parent(&self.nav.path))
    }

    /// Ask again, keeping the reader's place: a failed listing's way back.
    fn on_refresh(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        self.notice.clear();
        self.generation += 1;
        self.listing = Listing::Pending;
        self.column_listings.clear();
        Task::none()
    }

    fn on_load_more(&mut self) -> Task<Message> {
        if self.connected && self.listing.has_more() {
            self.pages += 1;
        }
        Task::none()
    }

    fn on_select(&mut self, path: String) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        let entry = self.entry_at(&path);
        let already = entry.path == self.selected;
        if entry.path.is_empty() || already {
            return Task::none();
        }
        self.choose(entry);
        Task::none()
    }

    /// A double-click or Enter: a folder becomes the directory, a file is
    /// chosen and previewed.
    fn on_open(&mut self, path: String) -> Task<Message> {
        let entry = self.entry_at(&path);
        match entry.is_dir() {
            true => self.on_navigate(path),
            false => self.on_select(path),
        }
    }

    /// The entry `path` names in any listing on screen.
    pub(super) fn entry_at(&self, path: &str) -> FsEntry {
        let here = crate::host::entry_named(self.listing.entries(), path);
        if !here.path.is_empty() {
            return here;
        }
        self.column_listings
            .values()
            .map(|listing| crate::host::entry_named(listing.entries(), path))
            .find(|entry| !entry.path.is_empty())
            .unwrap_or_default()
    }

    fn on_key_pressed(&mut self, key: BrowseKey) -> Task<Message> {
        let dismissing = key == BrowseKey::Cancel && !self.busy_writing();
        if dismissing {
            return match self.modal() {
                app_view::Modal::Naming => self.on_prompt(NamePrompt::Closed),
                app_view::Modal::Deleting => self.on_disarm_delete(),
                app_view::Modal::None => Task::none(),
            };
        }
        let modal = self.name_prompt.is_open() || !self.delete_target.is_empty();
        if modal || self.draft_here() {
            return Task::none();
        }
        match key {
            BrowseKey::Up => self.step_selection(-1),
            BrowseKey::Down => self.step_selection(1),
            BrowseKey::Left => self.step_left(),
            BrowseKey::Right => self.step_right(),
            BrowseKey::Open => self.open_selection(),
            BrowseKey::Parent => self.on_parent(),
            BrowseKey::Back => self.on_back(),
            BrowseKey::Forward => self.on_forward(),
            BrowseKey::Cancel | BrowseKey::Ignored => Task::none(),
        }
    }

    /// ↑/↓ move within the column that owns the choice; with nothing chosen
    /// the first row of the current directory is next.
    fn step_selection(&mut self, step: i64) -> Task<Message> {
        let column = self.column_owning(&self.selected).unwrap_or(0);
        let rows = self.rows_of_column(column);
        match browse::neighbour(&rows, &self.selected, step) {
            Some(path) => self.on_select(path),
            None => Task::none(),
        }
    }

    /// ← in column view chooses the folder that opened the choice's column;
    /// in list view it is nothing.
    fn step_left(&mut self) -> Task<Message> {
        let in_columns = self.view_mode == ViewMode::Columns;
        let Some(column) = self.column_owning(&self.selected).filter(|_| in_columns) else {
            return Task::none();
        };
        match column {
            0 => Task::none(),
            index => self.on_select(self.columns[index - 1].clone()),
        }
    }

    /// → in column view moves into the column the chosen folder opened;
    /// in list view it is nothing.
    fn step_right(&mut self) -> Task<Message> {
        let in_columns = self.view_mode == ViewMode::Columns;
        let opened = self
            .columns
            .iter()
            .position(|column| *column == self.selected);
        let Some(index) = opened.filter(|_| in_columns) else {
            return Task::none();
        };
        let rows = self.rows_of_column(index + 1);
        match rows.first() {
            Some(first) => self.on_select(first.path.clone()),
            None => Task::none(),
        }
    }

    fn open_selection(&mut self) -> Task<Message> {
        if self.selected.is_empty() {
            return Task::none();
        }
        self.on_open(self.selected.clone())
    }

    // ---- the pane ----

    fn on_set_view_mode(&mut self, mode: ViewMode) -> Task<Message> {
        self.view_mode = mode;
        self.columns.clear();
        self.column_listings.clear();
        let chosen = self.selected_entry();
        let reopen = mode == ViewMode::Columns && chosen.is_dir();
        if reopen {
            self.open_column(&chosen.path);
        }
        Task::none()
    }

    fn on_sort_by(&mut self, key: SortKey) -> Task<Message> {
        self.sort = self.sort.toggled(key);
        Task::none()
    }

    fn on_filter_changed(&mut self, filter: String) -> Task<Message> {
        self.filter = filter;
        Task::none()
    }

    fn on_toggle_sidebar(&mut self) -> Task<Message> {
        self.sidebar_open = !self.sidebar_open;
        let crowded = self.sidebar_open && self.viewport_width < BOTH_PANES_MIN;
        if crowded {
            self.inspector_open = false;
        }
        self.on_viewport_changed(self.viewport_width, self.viewport_height)
    }

    fn on_toggle_inspector(&mut self) -> Task<Message> {
        self.inspector_open = !self.inspector_open;
        self.on_viewport_changed(self.viewport_width, self.viewport_height)
    }

    // ---- the writes ----

    fn on_prompt(&mut self, prompt: NamePrompt) -> Task<Message> {
        let closing = !prompt.is_open();
        if closing {
            self.name_prompt = NamePrompt::Closed;
            self.name_draft.clear();
            return Task::none();
        }
        if !self.connected || self.loading() {
            return Task::none();
        }
        self.notice.clear();
        self.name_draft = match &prompt {
            NamePrompt::Rename(path) => crate::host::fs_name(path),
            NamePrompt::NewFolder | NamePrompt::NewFile | NamePrompt::Closed => String::new(),
        };
        self.name_prompt = prompt;
        let target = "FilesView/screen/name-prompt/name".to_owned();
        ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
            target: target.clone(),
        })
        .chain(ducktape_view_guest::widget::perform(
            wire::WidgetCommand::SelectAll { target },
        ))
    }

    fn on_name_changed(&mut self, name: String) -> Task<Message> {
        self.name_draft = name;
        Task::none()
    }

    /// The prompt's name leaves as the commit its kind means. The module's
    /// own write rule is checked first, so a directory nothing may be
    /// written in refuses before a signature.
    fn on_name_submit(&mut self) -> Task<Message> {
        let name = self.name_draft.trim().to_owned();
        let malformed = name.is_empty() || name.contains('/');
        if !self.connected || self.loading() || malformed {
            return Task::none();
        }
        let (act, refusal) = match &self.name_prompt {
            NamePrompt::Closed => return Task::none(),
            NamePrompt::NewFolder => (Act::Mkdir, self.refusal()),
            NamePrompt::NewFile => (Act::NewFile, self.refusal()),
            NamePrompt::Rename(from) => (
                Act::Rename,
                crate::host::write_refusal(&crate::host::fs_parent(from)),
            ),
        };
        if !refusal.is_empty() {
            self.notice = refusal;
            return Task::none();
        }
        self.notice.clear();
        self.writing = Writing::Busy(act);
        self.sent = match &self.name_prompt {
            NamePrompt::NewFolder => crate::host::make_dir(&self.nav.path, &name),
            NamePrompt::NewFile => crate::host::make_file(&self.nav.path, &name),
            NamePrompt::Rename(from) => {
                let to = crate::host::fs_child(&crate::host::fs_parent(from), &name);
                crate::host::move_object(from, &to)
            }
            NamePrompt::Closed => false,
        };
        Task::none()
    }

    fn on_arm_delete(&mut self, target: String) -> Task<Message> {
        if !self.connected || self.loading() {
            return Task::none();
        }
        self.delete_target = target;
        ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
            target: "FilesView/screen/confirm-delete".to_owned(),
        })
    }

    fn on_disarm_delete(&mut self) -> Task<Message> {
        self.delete_target.clear();
        Task::none()
    }

    fn on_delete_submit(&mut self) -> Task<Message> {
        if !self.connected || self.loading() || self.delete_target.is_empty() {
            return Task::none();
        }
        let refusal = crate::host::write_refusal(&crate::host::fs_parent(&self.delete_target));
        if !refusal.is_empty() {
            // the refusal is a notice behind the dialog's backdrop: the
            // dialog closes so the reader sees it
            self.notice = refusal;
            self.delete_target.clear();
            return Task::none();
        }
        self.notice.clear();
        self.writing = Writing::Busy(Act::Delete);
        self.sent = crate::host::delete_object(&self.delete_target);
        Task::none()
    }

    // ---- the history ----

    fn on_show_diff_of(&mut self, id: String) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        self.notice.clear();
        self.diff.clear();
        self.diff_omitted = 0;
        self.diff_from = id;
        self.inspector_open = true;
        self.on_viewport_changed(self.viewport_width, self.viewport_height)
    }

    fn on_close_diff(&mut self) -> Task<Message> {
        self.diff_from.clear();
        self.diff.clear();
        self.diff_omitted = 0;
        Task::none()
    }

    // ---- the editor ----

    fn on_begin_edit(&mut self, token: String) -> Task<Message> {
        let stale = token != self.edit_context();
        let not_text = self.preview.binary || self.preview.picture;
        let not_whole = self.preview.truncated;
        let not_read = self.preview.base.is_empty() || self.preview.path.is_empty();
        let unreadable = not_text || not_whole || not_read;
        let no_network = !self.connected || self.chain.is_empty();
        let mid_flight = self.editing || self.loading();
        if stale || unreadable || no_network || mid_flight {
            return Task::none();
        }
        self.editing = true;
        self.draft_id += 1;
        self.draft_chain = self.chain.clone();
        self.draft_path = self.preview.path.clone();
        self.draft_base = self.preview.base.clone();
        self.notice.clear();
        self.replace_draft(self.preview.text.clone());
        ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
            target: "FilesView/screen/inspector/info/preview/fs-editor".to_owned(),
        })
    }

    fn on_cancel_edit(&mut self, token: String) -> Task<Message> {
        if token != self.edit_context() {
            return Task::none();
        }
        self.close_draft();
        Task::none()
    }

    fn on_discard_draft(&mut self, id: i64) -> Task<Message> {
        if id != self.draft_id {
            return Task::none();
        }
        self.close_draft();
        Task::none()
    }

    fn close_draft(&mut self) {
        self.editing = false;
        if self.writing == Writing::Busy(Act::Save) {
            self.writing = Writing::Idle;
        }
        self.notice.clear();
        self.replace_draft(String::new());
    }

    fn replace_draft(&mut self, text: String) {
        let reset = self.draft.reset_revision();
        self.draft
            .replace(::ducktape_view_guest::Editor::new(text), reset);
    }

    /// Keep the original bytes and base until this exact draft is committed.
    fn on_save_edit(&mut self, token: String) -> Task<Message> {
        let stale = token != self.edit_context();
        let no_draft = !self.draft_here() || self.draft_base.is_empty();
        let cannot_send = self.loading() || !self.connected;
        if stale || no_draft || cannot_send {
            return Task::none();
        }
        self.notice.clear();
        self.writing = Writing::Busy(Act::Save);
        self.sent = crate::host::save(&self.draft_path, &self.draft_base, &self.draft.text());
        Task::none()
    }

    fn on_edit_draft(
        &mut self,
        document: ::ducktape_view_guest::EditorDocumentUpdate,
    ) -> Task<Message> {
        document.apply(&mut self.draft);
        Task::none()
    }

    fn on_draft_transaction(
        &mut self,
        transaction: ::ducktape_view_guest::EditorTransaction<Message>,
    ) -> Task<Message> {
        let route = transaction.apply(&mut self.draft);
        route.map_or_else(Task::none, Task::done)
    }

    // ---- the app's doors ----

    fn on_open_link_at(&mut self, url: String) -> Task<Message> {
        self.sent = crate::host::open_link(&url);
        Task::none()
    }

    // ---- the geometry ----

    fn on_sidebar_resized(&mut self, dx: f64) -> Task<Message> {
        self.sidebar_width = sidebar_width_within(
            self.sidebar_width + dx,
            self.viewport_width,
            if self.inspector_open {
                self.inspector_width
            } else {
                0.
            },
        );
        Task::none()
    }

    fn on_inspector_resized(&mut self, dx: f64) -> Task<Message> {
        self.inspector_width = inspector_width_within(
            self.inspector_width - dx,
            self.viewport_width,
            if self.sidebar_open {
                self.sidebar_width
            } else {
                0.
            },
        );
        Task::none()
    }

    fn on_column_resized(&mut self, index: usize, dx: f64) -> Task<Message> {
        if !dx.is_finite() {
            return Task::none();
        }
        self.column_widths.resize(
            self.column_widths.len().max(index + 1),
            columns::COLUMN_WIDTH,
        );
        self.column_widths[index] = (self.column_widths[index] + dx).clamp(160., 640.);
        Task::none()
    }

    fn on_chosen_resized(&mut self, dx: f64) -> Task<Message> {
        if !dx.is_finite() {
            return Task::none();
        }
        self.chosen_width = (self.chosen_width + dx).clamp(160., 640.);
        Task::none()
    }

    fn on_viewport_changed(&mut self, width: f64, height: f64) -> Task<Message> {
        self.viewport_width = width;
        self.viewport_height = height;
        // Close a rail before taking the filename column's space. Toggling
        // either rail on a small window makes room by closing the other.
        let both_crowded = self.sidebar_open && self.inspector_open && width < BOTH_PANES_MIN;
        if both_crowded {
            self.sidebar_open = false;
        }
        let inspector_crowded = self.inspector_open && width < INSPECTOR_MIN;
        if inspector_crowded {
            self.inspector_open = false;
        }
        let sidebar_crowded = self.sidebar_open && width < SIDEBAR_MIN;
        if sidebar_crowded {
            self.sidebar_open = false;
        }
        let inspector = if self.inspector_open {
            self.inspector_width
        } else {
            0.
        };
        self.sidebar_width = sidebar_width_within(self.sidebar_width, width, inspector);
        let sidebar = if self.sidebar_open {
            self.sidebar_width
        } else {
            0.
        };
        self.inspector_width = inspector_width_within(self.inspector_width, width, sidebar);
        Task::none()
    }
}

fn listing_of(read: crate::host::DirectoryRead) -> Listing {
    match read.error.is_empty() {
        true => Listing::Listed {
            entries: read.entries,
            next: read.next,
        },
        false => Listing::Failed(read.error),
    }
}

/// The main pane never drops under this, whatever the rails take.
const MAIN_PANE_MIN: f64 = 320.;
pub(super) const SIDEBAR_MIN: f64 = MAIN_PANE_MIN + 160. + 10.;
pub(super) const INSPECTOR_MIN: f64 = MAIN_PANE_MIN + 260. + 10.;
const BOTH_PANES_MIN: f64 = MAIN_PANE_MIN + 160. + 260. + 20.;

pub(crate) fn sidebar_width_within(width: f64, viewport: f64, inspector: f64) -> f64 {
    let other = if inspector > 0. { inspector + 10. } else { 0. };
    let maximum = (viewport - other - 10. - MAIN_PANE_MIN).clamp(160., 320.);
    width.clamp(160., maximum)
}

pub(crate) fn inspector_width_within(width: f64, viewport: f64, sidebar: f64) -> f64 {
    let other = if sidebar > 0. { sidebar + 10. } else { 0. };
    let maximum = (viewport - other - 10. - MAIN_PANE_MIN).clamp(260., 640.);
    width.clamp(260., maximum)
}
