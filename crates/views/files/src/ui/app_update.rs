use super::*;
impl super::FilesView {
    pub(crate) fn update(&mut self, message: Message) -> ducktape_view_guest::Task<Message> {
        match message {
            Message::TreeResized(dx, _dy) => self.on_tree_resized(dx, _dy),
            Message::PreviewResized(_dx, dy) => self.on_preview_resized(_dx, dy),
            Message::ObjectResized(dx, _dy) => self.on_object_resized(dx, _dy),
            Message::ViewportChanged(width, height) => self.on_viewport_changed(width, height),
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::RouteTo(target) => self.on_route_to(target),
            Message::ListingArrived(item) => self.on_listing_arrived(item),
            Message::PreviewArrived(item) => self.on_preview_arrived(item),
            Message::DiffArrived(item) => self.on_diff_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::OpenDirAt(target) => self.on_open_dir_at(target),
            Message::OpenFileAt(target) => self.on_open_file_at(target),
            Message::MkdirSubmit => self.on_mkdir_submit(),
            Message::NewFileSubmit => self.on_new_file_submit(),
            Message::ArmDeleteAt(target) => self.on_arm_delete_at(target),
            Message::DisarmDeleteNow => self.on_disarm_delete_now(),
            Message::DeleteSubmit => self.on_delete_submit(),
            Message::CloseDiffNow => self.on_close_diff_now(),
            Message::ShowDiffOf(id) => self.on_show_diff_of(id),
            Message::BeginEdit(token) => self.on_begin_edit(token),
            Message::CancelEdit(token) => self.on_cancel_edit(token),
            Message::DiscardDraft(id) => self.on_discard_draft(id),
            Message::SaveEdit(token) => self.on_save_edit(token),
            Message::OpenLinkAt(url) => self.on_open_link_at(url),
            Message::ToggleHistory => self.on_toggle_history(),
            Message::NewNameChanged(value) => self.on_new_name_changed(value),
            Message::DraftTransaction(transaction) => self.on_draft_transaction(transaction),
            Message::EditDraft(document) => self.on_edit_draft(document),
        }
    }
    fn on_tree_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.tree_width = crate::host::tree_width_after_delta(
            self.tree_width,
            dx,
            self.viewport_width,
            self.object_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_preview_resized(&mut self, _dx: f64, dy: f64) -> ducktape_view_guest::Task<Message> {
        self.preview_pane_height = crate::host::preview_height_after_delta(
            self.preview_pane_height,
            -dy,
            self.viewport_height,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_object_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.object_width = crate::host::object_width_after_delta(
            self.object_width,
            -dx,
            self.viewport_width,
            self.tree_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_viewport_changed(
        &mut self,
        width: f64,
        height: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.viewport_width = width;
        self.viewport_height = height;
        self.tree_width =
            crate::host::tree_width_after_delta(self.tree_width, 0.0, width, self.object_width);
        self.preview_pane_height =
            crate::host::preview_height_after_delta(self.preview_pane_height, 0.0, height);
        self.object_width =
            crate::host::object_width_after_delta(self.object_width, 0.0, width, self.tree_width);
        ::ducktape_view_guest::Task::none()
    }
    fn on_session_arrived(
        &mut self,
        item: crate::host::SessionItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.notice = crate::host::keep_str(
            !(item.error).is_empty(),
            ::std::convert::AsRef::as_ref(&(item.error)),
            ::std::convert::AsRef::as_ref(&(self.notice)),
        );
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let next = item.next.clone();
        self.generation =
            crate::host::generation_after(self.connected, next.connected, self.generation);
        {
            let next = self.listed && next.connected;
            if ::ducktape_view_guest::state_changed!(self.listed, next) {
                self.listed = next;
                self.derived.loading.take();
            }
        }
        {
            let next = next.connected;
            if ::ducktape_view_guest::state_changed!(self.connected, next) {
                self.connected = next;
                self.derived.loading.take();
            }
        }
        {
            let next = next.chain.to_owned();
            if ::ducktape_view_guest::state_changed!(self.chain, next) {
                self.chain = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
                self.derived.edit_context.take();
            }
        }
        self.dark = next.dark;
        let routed = (next.route_serial != self.route_serial) && (!(next.route).is_empty());
        self.route_serial = next.route_serial;
        let landing = crate::host::keep_str(
            routed,
            ::std::convert::AsRef::as_ref(&(next.route)),
            ::std::convert::AsRef::as_ref(&("")),
        );
        ducktape_view_guest::Task::done(Message::RouteTo(landing))
    }

    fn on_route_to(&mut self, target: String) -> ducktape_view_guest::Task<Message> {
        if (target).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        self.generation += 1;
        {
            let next = crate::host::fs_parent(::std::convert::AsRef::as_ref(&(target)));
            if ::ducktape_view_guest::state_changed!(self.path, next) {
                self.path = next;
                self.derived.refusal.take();
            }
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.listed, next) {
                self.listed = next;
                self.derived.loading.take();
            }
        }
        self.entries = Vec::new();
        self.directories = Vec::new();
        self.omitted = 0;
        self.diff_from = "".to_owned();
        self.diff = Vec::new();
        self.diff_omitted = 0;
        {
            let next = target.to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_path, next) {
                self.preview_path = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
                self.derived.edit_context.take();
            }
        }
        self.preview_entry = crate::host::no_fs_entry();
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_base, next) {
                self.preview_base = next;
                self.derived.edit_context.take();
            }
        }
        self.preview_text = "".to_owned();
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_display_text, next) {
                self.preview_display_text = next;
            }
        }
        self.preview_clipped = false;
        self.preview_truncated = false;
        self.preview_binary = false;
        self.preview_picture = false;
        self.preview_width = 0;
        self.preview_height = 0;
        self.sent = crate::host::at(::std::convert::AsRef::as_ref(&(self.path)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_listing_arrived(
        &mut self,
        item: crate::host::ListingItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.notice = crate::host::keep_str(
            !(item.error).is_empty(),
            ::std::convert::AsRef::as_ref(&(item.error)),
            ::std::convert::AsRef::as_ref(&(self.notice)),
        );
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.listed, next) {
                self.listed = next;
                self.derived.loading.take();
            }
        }
        self.entries = item.entries.clone();
        self.directories = item.directories.clone();
        self.history = item.history.clone();
        self.omitted = item.omitted;
        self.preview_entry = crate::host::entry_named(
            ::std::convert::AsRef::as_ref(&(item.entries)),
            ::std::convert::AsRef::as_ref(&(self.preview_path)),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_preview_arrived(
        &mut self,
        item: crate::host::PreviewItem,
    ) -> ducktape_view_guest::Task<Message> {
        if item.path != self.preview_path {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = crate::host::keep_str(
            !(item.error).is_empty(),
            ::std::convert::AsRef::as_ref(&(item.error)),
            ::std::convert::AsRef::as_ref(&(self.notice)),
        );
        if !(item.error).is_empty() {
            // a pane that never got its page would say "Reading…" forever:
            // it closes, the notice says why, and the row presses again
            {
                let next = "".to_owned();
                if ::ducktape_view_guest::state_changed!(self.preview_path, next) {
                    self.preview_path = next;
                    self.derived.draft_here.take();
                    self.derived.draft_parked.take();
                    self.derived.edit_context.take();
                }
            }
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = item.base.to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_base, next) {
                self.preview_base = next;
                self.derived.edit_context.take();
            }
        }
        self.preview_text = item.text.to_owned();
        {
            let next = item.display_text.to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_display_text, next) {
                self.preview_display_text = next;
            }
        }
        self.preview_clipped = item.clipped;
        self.preview_truncated = item.truncated;
        self.preview_binary = item.binary;
        self.preview_picture = item.picture;
        self.preview_width = item.width;
        self.preview_height = item.height;
        ::ducktape_view_guest::Task::none()
    }
    fn on_diff_arrived(
        &mut self,
        item: crate::host::DiffItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.notice = crate::host::keep_str(
            !(item.error).is_empty(),
            ::std::convert::AsRef::as_ref(&(item.error)),
            ::std::convert::AsRef::as_ref(&(self.notice)),
        );
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.diff = item.entries.clone();
        self.diff_omitted = item.omitted;
        ::ducktape_view_guest::Task::none()
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ducktape_view_guest::Task<Message> {
        self.notice = item.error.to_owned();
        let ok = (item.error).is_empty();
        let saved = item.kind == "save";
        let named = (item.kind == "mkdir") || (item.kind == "new_file");
        {
            let next = self.acting && saved;
            if ::ducktape_view_guest::state_changed!(self.acting, next) {
                self.acting = next;
                self.derived.loading.take();
            }
        }
        {
            let next = self.saving && (!saved);
            if ::ducktape_view_guest::state_changed!(self.saving, next) {
                self.saving = next;
                self.derived.loading.take();
            }
        }
        {
            let next = self.editing && (!(saved && ok));
            if ::ducktape_view_guest::state_changed!(self.editing, next) {
                self.editing = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        self.delete_target = crate::host::keep_str(
            (item.kind == "delete") && ok,
            ::std::convert::AsRef::as_ref(&("")),
            ::std::convert::AsRef::as_ref(&(self.delete_target)),
        );
        self.new_name =
            crate::host::keep_draft(named && ok, ::std::convert::AsRef::as_ref(&(self.new_name)));
        self.generation += 1;
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_dir_at(&mut self, target: String) -> ducktape_view_guest::Task<Message> {
        if ((*self.derived_loading()) || (!self.connected)) || (target == self.path) {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        {
            let next = target.to_owned();
            if ::ducktape_view_guest::state_changed!(self.path, next) {
                self.path = next;
                self.derived.refusal.take();
            }
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.listed, next) {
                self.listed = next;
                self.derived.loading.take();
            }
        }
        self.entries = Vec::new();
        self.directories = Vec::new();
        self.omitted = 0;
        self.diff_from = "".to_owned();
        self.diff = Vec::new();
        self.diff_omitted = 0;
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_path, next) {
                self.preview_path = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
                self.derived.edit_context.take();
            }
        }
        self.preview_entry = crate::host::no_fs_entry();
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_base, next) {
                self.preview_base = next;
                self.derived.edit_context.take();
            }
        }
        self.preview_text = "".to_owned();
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_display_text, next) {
                self.preview_display_text = next;
            }
        }
        self.preview_picture = false;
        self.preview_binary = false;
        self.preview_truncated = false;
        self.sent = crate::host::at(::std::convert::AsRef::as_ref(&(self.path)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_file_at(&mut self, target: String) -> ducktape_view_guest::Task<Message> {
        if ((*self.derived_loading()) || (!self.connected)) || (target == self.preview_path) {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        {
            let next = target.to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_path, next) {
                self.preview_path = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
                self.derived.edit_context.take();
            }
        }
        self.preview_entry = crate::host::entry_named(
            ::std::convert::AsRef::as_ref(&(self.entries)),
            ::std::convert::AsRef::as_ref(&(target)),
        );
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_base, next) {
                self.preview_base = next;
                self.derived.edit_context.take();
            }
        }
        self.preview_text = "".to_owned();
        {
            let next = "".to_owned();
            if ::ducktape_view_guest::state_changed!(self.preview_display_text, next) {
                self.preview_display_text = next;
            }
        }
        self.preview_clipped = false;
        self.preview_truncated = false;
        self.preview_binary = false;
        self.preview_picture = false;
        self.preview_width = 0;
        self.preview_height = 0;
        ::ducktape_view_guest::Task::none()
    }
    fn on_mkdir_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (((*self.derived_loading()) || (!self.connected))
            || ((self.new_name).trim().to_owned()).is_empty())
            || (!(*self.derived_refusal()).is_empty())
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.acting, next) {
                self.acting = next;
                self.derived.loading.take();
            }
        }
        self.sent = crate::host::make_dir(
            ::std::convert::AsRef::as_ref(&(self.path)),
            ::std::convert::AsRef::as_ref(&((self.new_name).trim().to_owned())),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_new_file_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (((*self.derived_loading()) || (!self.connected))
            || ((self.new_name).trim().to_owned()).is_empty())
            || (!(*self.derived_refusal()).is_empty())
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.acting, next) {
                self.acting = next;
                self.derived.loading.take();
            }
        }
        self.sent = crate::host::make_file(
            ::std::convert::AsRef::as_ref(&(self.path)),
            ::std::convert::AsRef::as_ref(&((self.new_name).trim().to_owned())),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_arm_delete_at(&mut self, target: String) -> ducktape_view_guest::Task<Message> {
        self.delete_target = target.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_disarm_delete_now(&mut self) -> ducktape_view_guest::Task<Message> {
        self.delete_target = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_delete_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((*self.derived_loading()) || (!self.connected)) || (self.delete_target).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = crate::host::write_refusal(::std::convert::AsRef::as_ref(
            &(crate::host::fs_parent(::std::convert::AsRef::as_ref(&(self.delete_target)))),
        ));
        if !(self.notice).is_empty() {
            // the refusal is a notice behind the dialog's backdrop: the
            // dialog closes so the reader sees it
            self.delete_target = "".to_owned();
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.acting, next) {
                self.acting = next;
                self.derived.loading.take();
            }
        }
        self.sent =
            crate::host::delete_object(::std::convert::AsRef::as_ref(&(self.delete_target)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_close_diff_now(&mut self) -> ducktape_view_guest::Task<Message> {
        self.diff_from = "".to_owned();
        self.diff = Vec::new();
        self.diff_omitted = 0;
        ::ducktape_view_guest::Task::none()
    }
    fn on_show_diff_of(&mut self, id: String) -> ducktape_view_guest::Task<Message> {
        if (*self.derived_loading()) || (!self.connected) {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        self.diff = Vec::new();
        self.diff_omitted = 0;
        self.diff_from = id.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_begin_edit(&mut self, token: String) -> ducktape_view_guest::Task<Message> {
        if (((((((((token != (*self.derived_edit_context())) || self.editing)
            || (*self.derived_loading()))
            || (!self.connected))
            || (self.chain).is_empty())
            || (self.preview_base).is_empty())
            || self.preview_binary)
            || self.preview_picture)
            || self.preview_truncated)
            || (self.preview_path).is_empty()
        {
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.editing, next) {
                self.editing = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        {
            let next = self.draft_id + 1;
            if ::ducktape_view_guest::state_changed!(self.draft_id, next) {
                self.draft_id = next;
                self.derived.edit_context.take();
            }
        }
        {
            let next = self.chain.to_owned();
            if ::ducktape_view_guest::state_changed!(self.draft_chain, next) {
                self.draft_chain = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        {
            let next = self.preview_path.to_owned();
            if ::ducktape_view_guest::state_changed!(self.draft_path, next) {
                self.draft_path = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        self.draft_base = self.preview_base.to_owned();
        self.notice = "".to_owned();
        {
            let reset = self.draft.reset_revision();
            let next = ::ducktape_view_guest::Editor::new(self.preview_text.to_owned());
            self.draft.replace(next, reset);
        };
        ::ducktape_view_guest::Task::none()
    }
    fn on_cancel_edit(&mut self, token: String) -> ducktape_view_guest::Task<Message> {
        if token != (*self.derived_edit_context()) {
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.editing, next) {
                self.editing = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.saving, next) {
                self.saving = next;
                self.derived.loading.take();
            }
        }
        self.notice = "".to_owned();
        {
            let reset = self.draft.reset_revision();
            let next = ::ducktape_view_guest::Editor::new("".to_owned());
            self.draft.replace(next, reset);
        };
        ::ducktape_view_guest::Task::none()
    }
    fn on_discard_draft(&mut self, id: i64) -> ducktape_view_guest::Task<Message> {
        if id != self.draft_id {
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.editing, next) {
                self.editing = next;
                self.derived.draft_here.take();
                self.derived.draft_parked.take();
            }
        }
        {
            let next = false;
            if ::ducktape_view_guest::state_changed!(self.saving, next) {
                self.saving = next;
                self.derived.loading.take();
            }
        }
        self.notice = "".to_owned();
        {
            let reset = self.draft.reset_revision();
            let next = ::ducktape_view_guest::Editor::new("".to_owned());
            self.draft.replace(next, reset);
        };
        ::ducktape_view_guest::Task::none()
    }
    fn on_save_edit(&mut self, token: String) -> ducktape_view_guest::Task<Message> {
        if ((((token != (*self.derived_edit_context())) || (!(*self.derived_draft_here())))
            || (*self.derived_loading()))
            || (!self.connected))
            || (self.draft_base).is_empty()
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.notice = "".to_owned();
        {
            let next = true;
            if ::ducktape_view_guest::state_changed!(self.saving, next) {
                self.saving = next;
                self.derived.loading.take();
            }
        }
        self.sent = crate::host::save(
            ::std::convert::AsRef::as_ref(&(self.draft_path)),
            ::std::convert::AsRef::as_ref(&(self.draft_base)),
            ::std::convert::AsRef::as_ref(&((self.draft).text())),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_link_at(&mut self, url: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::open_link(::std::convert::AsRef::as_ref(&(url)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_toggle_history(&mut self) -> ducktape_view_guest::Task<Message> {
        self.history_open = !self.history_open;
        ::ducktape_view_guest::Task::none()
    }
    fn on_new_name_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.new_name = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_draft_transaction(
        &mut self,
        transaction: ::ducktape_view_guest::EditorTransaction<Message>,
    ) -> ducktape_view_guest::Task<Message> {
        let route = transaction.apply(&mut self.draft);
        route.map_or_else(
            ::ducktape_view_guest::Task::none,
            ::ducktape_view_guest::Task::done,
        )
    }
    fn on_edit_draft(
        &mut self,
        document: ::ducktape_view_guest::EditorDocumentUpdate,
    ) -> ducktape_view_guest::Task<Message> {
        document.apply(&mut self.draft);
        ::ducktape_view_guest::Task::none()
    }
}
