impl PagesView {
    fn refresh_document_presentation(&mut self) {
        self.document_paint = crate::editor_view::document_presentation(
            &self.document,
            self.document_menu.clone(),
            self.document_dark,
            self.document_commented.clone(),
            self.document_marks.clone(),
            self.document_focused,
            self.document_reserve,
        );
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::CommentPointerMoved(_x, y) => self.on_comment_pointer_moved(_x, y),
            Message::ChoosePage(id) => self.on_choose_page(id),
            Message::RegisterArrived(item) => self.on_register_arrived(item),
            Message::SearchArrived(item) => self.on_search_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::SaveDone(item) => self.on_save_done(item),
            Message::PageAutosaveTick => self.on_page_autosave_tick(),
            Message::TogglePageCreate => self.on_toggle_page_create(),
            Message::CreatePageSubmit => self.on_create_page_submit(),
            Message::ArmPageDelete => self.on_arm_page_delete(),
            Message::DisarmPageDelete => self.on_disarm_page_delete(),
            Message::DeletePageSubmit => self.on_delete_page_submit(),
            Message::SearchPagesSubmit => self.on_search_pages_submit(),
            Message::ClearPageSearch => self.on_clear_page_search(),
            Message::OpenPageSearchHit(page_id, _block_id) => {
                self.on_open_page_search_hit(page_id, _block_id)
            }
            Message::UseOrphanedCommentDraft(draft) => self.on_use_orphaned_comment_draft(draft),
            Message::DiscardOrphanedCommentDraft(draft) => {
                self.on_discard_orphaned_comment_draft(draft)
            }
            Message::ToggleBlockComments => self.on_toggle_block_comments(),
            Message::CloseBlockComments => self.on_close_block_comments(),
            Message::NarrowCommentScope(target) => self.on_narrow_comment_scope(target),
            Message::WidenCommentScope => self.on_widen_comment_scope(),
            Message::ResolveThreadSubmit(id, resolved) => {
                self.on_resolve_thread_submit(id, resolved)
            }
            Message::SelectReplyThread(id) => self.on_select_reply_thread(id),
            Message::ToggleThreadReplies(id) => self.on_toggle_thread_replies(id),
            Message::ToggleResolvedComments => self.on_toggle_resolved_comments(),
            Message::PostThreadReply(id) => self.on_post_thread_reply(id),
            Message::PostBlockCommentSubmit => self.on_post_block_comment_submit(),
            Message::BeginEditComment(id, text) => self.on_begin_edit_comment(id, text),
            Message::CancelEditComment => self.on_cancel_edit_comment(),
            Message::SubmitEditComment => self.on_submit_edit_comment(),
            Message::DeleteCommentSubmit(id) => self.on_delete_comment_submit(id),
            Message::CommentEditDraftChanged(value) => self.on_comment_edit_draft_changed(value),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::DocumentPointerReleased(_button) => self.on_document_pointer_released(_button),
            Message::DocumentKeyReleased(_key) => self.on_document_key_released(_key),
            Message::DocumentWindowFocused => self.on_document_window_focused(),
            Message::DocumentWindowUnfocused => self.on_document_window_unfocused(),
            Message::DocumentFocusChecked(query, focused) => {
                self.on_document_focus_checked(query, focused)
            }
            Message::SidebarResized(dx, _dy) => self.on_sidebar_resized(dx, _dy),
            Message::PagesViewportChanged(width, _height) => {
                self.on_pages_viewport_changed(width, _height)
            }
            Message::PagesPaneResized(width, _height) => self.on_pages_pane_resized(width, _height),
            Message::CommentsCardMeasured(_width, height) => {
                self.on_comments_card_measured(_width, height)
            }
            Message::TogglePageMenu => self.on_toggle_page_menu(),
            Message::ClosePageMenu => self.on_close_page_menu(),
            Message::DocumentCommitted(next) => self.on_document_committed(next),
            Message::PageDraftChanged(value) => self.on_page_draft_changed(value),
            Message::SearchDraftChanged(value) => self.on_search_draft_changed(value),
            Message::ReplyDraftChanged(value) => self.on_reply_draft_changed(value),
            Message::CommentDraftChanged(value) => self.on_comment_draft_changed(value),
            Message::DocumentTransaction(transaction) => self.on_document_transaction(transaction),
            Message::DocumentUpdated(document) => self.on_document_updated(document),
        }
    }
    fn on_session_arrived(&mut self, item: crate::host::SessionItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        let next = item.next.clone();
        self.register_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.register_serial,
        );
        self.connected = next.connected;
        self.chain = next.chain.to_owned();
        self.document_dark = next.dark;
        self.refresh_document_presentation();
        let route_moved = (crate::host::route_arrived(next.route_serial, self.route_serial)
            && (!(next.route_page).is_empty()))
            && (next.route_page != self.active_page);
        self.route_serial = next.route_serial;
        self.active_page =
            crate::host::keep_str(route_moved, &(next.route_page), &(self.active_page));
        self.loading = self.loading || route_moved;
        self.page_link = crate::host::page_address(&(self.active_page), &(self.chain));
        Task::none()
    }
    fn on_comment_pointer_moved(&mut self, _x: f64, y: f64) -> Task<Message> {
        self.pointer_y = y;
        Task::none()
    }
    fn on_choose_page(&mut self, id: String) -> Task<Message> {
        if (!(self.host_error).is_empty()) || (id).is_empty() {
            return Task::none();
        }
        if self.loading || self.busy {
            return Task::none();
        }
        if id == self.active_page {
            return Task::none();
        }
        self.active_page = id.to_owned();
        self.active_page_title =
            crate::host::page_display_title(&(self.pages), &(id), &(self.active_page_title));
        self.active_page_parent = "".to_owned();
        self.blocks = Vec::new();
        self.page_link = crate::host::page_address(&(id), &(self.chain));
        self.loading = true;
        Task::none()
    }
    fn on_register_arrived(&mut self, item: crate::host::RegisterItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        self.loading = false;
        if !(item.error).is_empty() {
            return Task::none();
        }
        let page_moved = item.active_page != self.buffer_page;
        let comments_carry = self.block_comments_open && (!page_moved);
        self.orphaned_comment_drafts = crate::host::remember_draft(
            &(self.orphaned_comment_drafts),
            &(crate::host::keep_str(page_moved, &(self.block_comment_draft), "")),
        );
        self.block_comment_draft =
            crate::host::keep_str(page_moved, "", &(self.block_comment_draft));
        self.block_comments_open = comments_carry;
        self.comment_anchor_line =
            crate::host::keep_i64(comments_carry, self.comment_anchor_line, 0);
        self.document_reserve = crate::host::comments_reserve(
            self.pages_pane_width,
            comments_carry,
            self.comment_anchor_line,
            self.comments_card_height,
        );
        self.scope_target = crate::host::keep_str(comments_carry, &(self.scope_target), "");
        self.scope_pinned = self.scope_pinned && comments_carry;
        self.reply_thread = crate::host::keep_str(comments_carry, &(self.reply_thread), "");
        self.reply_draft = crate::host::keep_str(comments_carry, &(self.reply_draft), "");
        self.expanded_threads =
            crate::host::kept_ids(comments_carry, ::std::mem::take(&mut self.expanded_threads));
        self.resolved_open = self.resolved_open && comments_carry;
        self.page_searching = self.page_searching && (!page_moved);
        self.page_search_query = crate::host::keep_str(page_moved, "", &(self.page_search_query));
        self.page_delete_armed = self.page_delete_armed && (!page_moved);
        self.autosave = crate::host::keep_str(page_moved, "idle", &(self.autosave));
        self.pages = item.pages.clone();
        self.blocks = item.blocks.clone();
        self.subpages = item.subpages.clone();
        self.active_page_title = item.active_page_title.to_owned();
        self.active_page_parent = item.active_page_parent.to_owned();
        self.thread_total = item.thread_total;
        self.comment_rows = item.comment_rows.clone();
        self.commented_hits = item.commented_hits.clone();
        self.member_names = item.names.clone();
        self.member_agents = item.agents.clone();
        self.threads_loading = false;
        self.document_commented =
            crate::host::commented_lines(&(item.blocks), &(item.commented_hits));
        self.document_marks = crate::host::comment_marks(&(item.blocks), &(item.commented_hits));
        self.page_link = crate::host::page_address(&(item.active_page), &(self.chain));
        let install = crate::host::install_decision(
            &(crate::host::document_text(&(self.document))),
            &(self.buffer_page),
            &(item.active_page),
            &(self.page_saved_text),
            &(item.document),
        );
        self.active_page = item.active_page.to_owned();
        self.buffer_page = item.active_page.to_owned();
        self.refresh_document_presentation();
        if !install {
            return Task::none();
        }
        self.page_saved_text = item.document.to_owned();
        self.page_refusal = "".to_owned();
        {
            let reset = self.document.reset_revision();
            let next = crate::host::document_editor(&(item.document));
            self.document.replace(next, reset);
        };
        self.document_menu = crate::editor_binding::initial_menu();
        self.document_focused = false;
        self.document_error = "".to_owned();
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_search_arrived(&mut self, item: crate::host::SearchItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        self.page_searching = false;
        if item.query != self.page_search_query {
            return Task::none();
        }
        self.page_search_hits = item.hits.clone();
        Task::none()
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> Task<Message> {
        self.busy = false;
        self.threads_loading = false;
        self.host_error = item.error.to_owned();
        self.register_serial += 1;
        let refused = !(item.error).is_empty();
        self.page_draft = crate::host::keep_str(refused, &(self.pending_page), &(self.page_draft));
        self.reply_draft = crate::host::keep_str(
            refused && (!(self.reply_thread).is_empty()),
            &(self.pending_comment),
            &(self.reply_draft),
        );
        self.block_comment_draft = crate::host::keep_str(
            refused && (self.reply_thread).is_empty(),
            &(self.pending_comment),
            &(self.block_comment_draft),
        );
        self.pending_page = "".to_owned();
        self.pending_comment = "".to_owned();
        if refused {
            return Task::none();
        }
        self.page_create_open = false;
        self.page_delete_armed = false;
        self.active_page =
            crate::host::keep_str(!(item.page).is_empty(), &(item.page), &(self.active_page));
        Task::none()
    }
    fn on_save_done(&mut self, item: crate::host::SaveItem) -> Task<Message> {
        self.autosave = "error".to_owned();
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        self.host_error = "".to_owned();
        self.page_refusal = item.refusal.to_owned();
        self.page_saved_text = crate::host::baseline_at_submitted_title(
            &(crate::host::saved_baseline(
                item.written,
                &(item.document),
                &(self.page_inflight_text),
            )),
            &(self.page_inflight_text),
        );
        self.autosave = "saved".to_owned();
        self.register_serial += crate::host::keep_i64(item.written, 1, 0);
        if (item.refusal).is_empty() {
            return Task::none();
        }
        let untouched = crate::host::document_text(&(self.document)) == self.page_inflight_text;
        self.page_saved_text =
            crate::host::baseline_at_submitted_title(&(item.document), &(self.page_inflight_text));
        self.autosave = "idle".to_owned();
        if !untouched {
            return Task::none();
        }
        {
            let reset = self.document.reset_revision();
            let next = crate::host::document_editor(&(item.document));
            self.document.replace(next, reset);
        };
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_page_autosave_tick(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if ((self.busy || self.loading) || (self.active_page).is_empty())
            || (self.active_page != self.buffer_page)
        {
            return Task::none();
        }
        if self.autosave == "saving" {
            return Task::none();
        }
        let text = crate::host::document_text(&(self.document));
        if text == self.page_saved_text {
            return Task::none();
        }
        self.autosave = "idle".to_owned();
        if crate::host::has_unclosed_fence(&(text)) {
            return Task::none();
        }
        self.autosave = "saving".to_owned();
        self.page_inflight_text = text.to_owned();
        crate::host::save(&(self.active_page), &(text), &(self.page_saved_text));
        Task::none()
    }
    fn on_toggle_page_create(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        self.page_create_open = !self.page_create_open;
        Task::none()
    }
    fn on_create_page_submit(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if ((self.loading || self.busy) || (!self.connected))
            || ((self.page_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        self.busy = true;
        self.pending_page = (self.page_draft).trim().to_owned();
        self.page_draft = "".to_owned();
        crate::host::create(&(self.pending_page));
        Task::none()
    }
    fn on_arm_page_delete(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (self.loading || self.busy) || (self.active_page).is_empty() {
            return Task::none();
        }
        self.page_menu_open = false;
        self.page_delete_armed = true;
        Task::none()
    }
    fn on_disarm_page_delete(&mut self) -> Task<Message> {
        self.page_delete_armed = false;
        Task::none()
    }
    fn on_delete_page_submit(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if ((self.loading || self.busy) || (self.active_page).is_empty())
            || (!self.page_delete_armed)
        {
            return Task::none();
        }
        self.busy = true;
        self.page_delete_armed = false;
        self.orphaned_comment_drafts = crate::host::remember_draft(
            &(self.orphaned_comment_drafts),
            &(self.block_comment_draft),
        );
        self.block_comment_draft = "".to_owned();
        crate::host::delete(&(self.active_page));
        Task::none()
    }
    fn on_search_pages_submit(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if self.page_searching || ((self.page_search_draft).trim().to_owned()).is_empty() {
            return Task::none();
        }
        self.page_searching = true;
        self.page_search_hits = Vec::new();
        self.page_search_query = (self.page_search_draft).trim().to_owned();
        self.page_search_serial += 1;
        Task::none()
    }
    fn on_clear_page_search(&mut self) -> Task<Message> {
        self.page_search_draft = "".to_owned();
        self.page_search_hits = Vec::new();
        self.page_searching = false;
        self.page_search_query = "".to_owned();
        Task::none()
    }
    fn on_open_page_search_hit(&mut self, page_id: String, _block_id: String) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if self.loading || self.busy {
            return Task::none();
        }
        (Task::done(page_id.to_owned())).map(Message::ChoosePage)
    }
    fn on_use_orphaned_comment_draft(&mut self, draft: String) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (self.loading || self.busy)
            || (!((self.block_comment_draft).trim().to_owned()).is_empty())
        {
            return Task::none();
        }
        self.block_comment_draft = draft.to_owned();
        self.block_comments_open = true;
        self.scope_target = "".to_owned();
        self.scope_pinned = false;
        self.orphaned_comment_drafts =
            crate::host::forget_draft(&(self.orphaned_comment_drafts), &(draft));
        self.comment_anchor_line = 0;
        let recovered = crate::host::comments_reserve(
            self.pages_pane_width,
            true,
            0,
            self.comments_card_height,
        );
        if (recovered.line == self.document_reserve.line)
            && (recovered.height == self.document_reserve.height)
        {
            return Task::none();
        }
        self.document_reserve = recovered;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_discard_orphaned_comment_draft(&mut self, draft: String) -> Task<Message> {
        self.orphaned_comment_drafts =
            crate::host::forget_draft(&(self.orphaned_comment_drafts), &(draft));
        Task::none()
    }
    fn on_toggle_block_comments(&mut self) -> Task<Message> {
        self.comment_anchor_y = -1.0;
        self.comment_anchor_line = 0;
        self.comment_anchor_range = None;
        self.comment_mention = 0;
        self.comment_edit_id = "".to_owned();
        self.comment_edit_draft = "".to_owned();
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (self.loading || self.busy) || (self.active_page).is_empty() {
            return Task::none();
        }
        self.orphaned_comment_drafts = crate::host::remember_draft(
            &(self.orphaned_comment_drafts),
            &(self.block_comment_draft),
        );
        self.block_comment_draft = "".to_owned();
        self.block_comments_open = !self.block_comments_open;
        self.scope_target = "".to_owned();
        self.scope_pinned = false;
        self.reply_thread = "".to_owned();
        self.reply_draft = "".to_owned();
        self.resolved_open = false;
        let toggled_reserve = crate::host::comments_reserve(
            self.pages_pane_width,
            self.block_comments_open,
            0,
            self.comments_card_height,
        );
        if (toggled_reserve.line == self.document_reserve.line)
            && (toggled_reserve.height == self.document_reserve.height)
        {
            return Task::none();
        }
        self.document_reserve = toggled_reserve;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_close_block_comments(&mut self) -> Task<Message> {
        self.comment_anchor_y = -1.0;
        self.comment_anchor_line = 0;
        self.comment_anchor_range = None;
        self.comment_mention = 0;
        self.comment_edit_id = "".to_owned();
        self.comment_edit_draft = "".to_owned();
        self.orphaned_comment_drafts = crate::host::remember_draft(
            &(self.orphaned_comment_drafts),
            &(self.block_comment_draft),
        );
        self.block_comment_draft = "".to_owned();
        self.block_comments_open = false;
        self.scope_target = "".to_owned();
        self.scope_pinned = false;
        self.reply_thread = "".to_owned();
        self.reply_draft = "".to_owned();
        self.resolved_open = false;
        if self.document_reserve.height == 0 {
            return Task::none();
        }
        self.document_reserve = crate::editor_view::no_reserve();
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_narrow_comment_scope(&mut self, target: String) -> Task<Message> {
        self.reply_thread = "".to_owned();
        self.reply_draft = "".to_owned();
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (self.loading || self.busy) || (target).is_empty() {
            return Task::none();
        }
        self.scope_target = target.to_owned();
        Task::none()
    }
    fn on_widen_comment_scope(&mut self) -> Task<Message> {
        self.reply_thread = "".to_owned();
        self.reply_draft = "".to_owned();
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (self.loading || self.busy) || self.scope_pinned {
            return Task::none();
        }
        self.scope_target = "".to_owned();
        Task::none()
    }
    fn on_resolve_thread_submit(&mut self, id: String, resolved: bool) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (((self.loading || self.busy) || self.threads_loading) || (!self.block_comments_open))
            || (id).is_empty()
        {
            return Task::none();
        }
        self.busy = true;
        self.threads_loading = true;
        crate::host::resolve(&(id), resolved);
        Task::none()
    }
    fn on_select_reply_thread(&mut self, id: String) -> Task<Message> {
        self.reply_draft = "".to_owned();
        self.reply_thread = crate::host::reply_thread_after_press(&(self.reply_thread), &(id));
        Task::none()
    }
    fn on_toggle_thread_replies(&mut self, id: String) -> Task<Message> {
        self.expanded_threads =
            crate::host::toggled(::std::mem::take(&mut self.expanded_threads), &(id));
        Task::none()
    }
    fn on_toggle_resolved_comments(&mut self) -> Task<Message> {
        self.resolved_open = !self.resolved_open;
        Task::none()
    }
    fn on_post_thread_reply(&mut self, id: String) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if (((self.loading || self.busy) || self.threads_loading) || (!self.block_comments_open))
            || ((self.reply_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        let reply_target =
            crate::host::comment_post_target(&(self.comment_rows), &(id), &(self.scope_target));
        if (reply_target).is_empty() {
            return Task::none();
        }
        self.busy = true;
        self.threads_loading = true;
        self.pending_comment = (self.reply_draft).trim().to_owned();
        self.reply_draft = "".to_owned();
        crate::host::post(&(self.pending_comment), &(reply_target), &(id), None, 0);
        Task::none()
    }
    fn on_post_block_comment_submit(&mut self) -> Task<Message> {
        if !(self.host_error).is_empty() {
            return Task::none();
        }
        if ((((self.loading || self.busy) || self.threads_loading) || (!self.block_comments_open))
            || (self.active_page).is_empty())
            || ((self.block_comment_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        let fresh_target = crate::host::keep_str(
            !(self.scope_target).is_empty(),
            &(self.scope_target),
            &(self.active_page),
        );
        self.busy = true;
        self.threads_loading = true;
        self.pending_comment = (self.block_comment_draft).trim().to_owned();
        self.block_comment_draft = "".to_owned();
        // A selection-anchored thread pins to its text once; the next
        // composer post is on the whole scope again.
        let anchor = self.comment_anchor_range.take();
        let mention = std::mem::take(&mut self.comment_mention);
        crate::host::post(&(self.pending_comment), &(fresh_target), "", anchor, mention);
        Task::none()
    }
    /// ONE COMMENT IS REWRITTEN AT A TIME: opening another box drops the last
    /// draft, and a second press on the same comment puts the box away.
    fn on_begin_edit_comment(&mut self, id: String, text: String) -> Task<Message> {
        if !self.host_error.is_empty() || self.loading || self.busy || self.threads_loading {
            return Task::none();
        }
        let same = self.comment_edit_id == id;
        self.comment_edit_id = if same { String::new() } else { id };
        self.comment_edit_draft = if same { String::new() } else { text };
        Task::none()
    }
    fn on_cancel_edit_comment(&mut self) -> Task<Message> {
        self.comment_edit_id = "".to_owned();
        self.comment_edit_draft = "".to_owned();
        Task::none()
    }
    fn on_submit_edit_comment(&mut self) -> Task<Message> {
        let blocked = !self.host_error.is_empty()
            || self.loading
            || self.busy
            || self.threads_loading
            || self.comment_edit_id.is_empty()
            || self.comment_edit_draft.trim().is_empty();
        if blocked {
            return Task::none();
        }
        self.busy = true;
        self.threads_loading = true;
        crate::host::edit_comment(&self.comment_edit_id, &self.comment_edit_draft);
        self.comment_edit_id = "".to_owned();
        self.comment_edit_draft = "".to_owned();
        Task::none()
    }
    fn on_delete_comment_submit(&mut self, id: String) -> Task<Message> {
        let blocked = !self.host_error.is_empty()
            || self.loading
            || self.busy
            || self.threads_loading
            || id.is_empty();
        if blocked {
            return Task::none();
        }
        self.busy = true;
        self.threads_loading = true;
        if self.comment_edit_id == id {
            self.comment_edit_id = "".to_owned();
            self.comment_edit_draft = "".to_owned();
        }
        crate::host::delete_comment(&id);
        Task::none()
    }
    fn on_comment_edit_draft_changed(&mut self, value: String) -> Task<Message> {
        self.comment_edit_draft = value;
        Task::none()
    }
    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<Message> {
        crate::host::copy(&(text), &(label));
        Task::none()
    }
    fn on_document_pointer_released(&mut self, _button: wire::mouse::Button) -> Task<Message> {
        self.focus_query += 1;
        let query = self.focus_query;
        ::ducktape_view_guest::widget::is_focused(String::from("PagesView/root/pages/document"))
            .map(move |value| Message::DocumentFocusChecked(query, value))
    }
    fn on_document_key_released(&mut self, _key: KeyRelease) -> Task<Message> {
        self.focus_query += 1;
        let query = self.focus_query;
        ::ducktape_view_guest::widget::is_focused(String::from("PagesView/root/pages/document"))
            .map(move |value| Message::DocumentFocusChecked(query, value))
    }
    fn on_document_window_focused(&mut self) -> Task<Message> {
        self.focus_query += 1;
        let query = self.focus_query;
        ::ducktape_view_guest::widget::is_focused(String::from("PagesView/root/pages/document"))
            .map(move |value| Message::DocumentFocusChecked(query, value))
    }
    fn on_document_window_unfocused(&mut self) -> Task<Message> {
        self.focus_query += 1;
        self.document_focused = false;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_document_focus_checked(&mut self, query: i64, focused: bool) -> Task<Message> {
        if (query != self.focus_query) || (focused == self.document_focused) {
            return Task::none();
        }
        self.document_focused = focused;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_sidebar_resized(&mut self, dx: f64, _dy: f64) -> Task<Message> {
        self.sidebar_width = crate::host::sidebar_width_after_delta(
            self.sidebar_width,
            dx,
            self.pages_viewport_width,
        );
        Task::none()
    }
    fn on_pages_viewport_changed(&mut self, width: f64, _height: f64) -> Task<Message> {
        self.pages_viewport_width = width;
        self.sidebar_width = crate::host::sidebar_width_after_delta(self.sidebar_width, 0.0, width);
        Task::none()
    }
    fn on_pages_pane_resized(&mut self, width: f64, _height: f64) -> Task<Message> {
        self.pages_pane_width = width;
        let next = crate::host::comments_reserve(
            width,
            self.block_comments_open,
            self.comment_anchor_line,
            self.comments_card_height,
        );
        if (next.line == self.document_reserve.line)
            && (next.height == self.document_reserve.height)
        {
            return Task::none();
        }
        self.document_reserve = next;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_comments_card_measured(&mut self, _width: f64, height: f64) -> Task<Message> {
        let measured = crate::host::measured_card_height(self.comments_card_height, height);
        if measured == self.comments_card_height {
            return Task::none();
        }
        self.comments_card_height = measured;
        let next = crate::host::comments_reserve(
            self.pages_pane_width,
            self.block_comments_open,
            self.comment_anchor_line,
            measured,
        );
        if (next.line == self.document_reserve.line)
            && (next.height == self.document_reserve.height)
        {
            return Task::none();
        }
        self.document_reserve = next;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_toggle_page_menu(&mut self) -> Task<Message> {
        self.page_menu_open = !self.page_menu_open;
        Task::none()
    }
    fn on_close_page_menu(&mut self) -> Task<Message> {
        self.page_menu_open = false;
        Task::none()
    }
    fn on_document_committed(
        &mut self,
        next: crate::editor_binding::EditorUpdate,
    ) -> Task<Message> {
        self.document_history = next.history.clone();
        self.document_menu = next.menu.clone();
        self.refresh_document_presentation();
        let comment_line = crate::host::navigation_comment_line(next.interaction.clone());
        self.page_refusal = "".to_owned();
        crate::host::open_link(&(crate::host::navigation_link(next.interaction.clone())));
        crate::host::copy(
            &crate::host::navigation_copy(next.interaction.clone()),
            "Copied",
        );
        if (((comment_line < 0) || self.loading) || self.busy) || (self.active_page).is_empty() {
            return Task::none();
        }
        // A comment from the format menu pins to the words that were
        // selected; a badge or a block menu comments on the block as a whole.
        self.comment_anchor_range = crate::host::navigation_anchor(next.interaction.clone());
        self.comment_mention = crate::host::navigation_mention(next.interaction.clone());
        self.orphaned_comment_drafts = crate::host::remember_draft(
            &(self.orphaned_comment_drafts),
            &(self.block_comment_draft),
        );
        self.block_comment_draft = "".to_owned();
        self.reply_thread = "".to_owned();
        self.reply_draft = "".to_owned();
        self.resolved_open = false;
        self.scope_target = crate::host::block_at_line(&(self.blocks), comment_line);
        self.scope_pinned = !(self.scope_target).is_empty();
        self.comment_anchor_y = self.pointer_y;
        self.block_comments_open = true;
        self.comment_anchor_line = comment_line;
        let opened = crate::host::comments_reserve(
            self.pages_pane_width,
            true,
            comment_line,
            self.comments_card_height,
        );
        if (opened.line == self.document_reserve.line)
            && (opened.height == self.document_reserve.height)
        {
            return Task::none();
        }
        self.document_reserve = opened;
        self.refresh_document_presentation();
        Task::none()
    }
    fn on_page_draft_changed(&mut self, value: String) -> Task<Message> {
        self.page_draft = value;
        Task::none()
    }
    fn on_search_draft_changed(&mut self, value: String) -> Task<Message> {
        self.page_search_draft = value;
        Task::none()
    }
    fn on_reply_draft_changed(&mut self, value: String) -> Task<Message> {
        self.reply_draft = value;
        Task::none()
    }
    fn on_comment_draft_changed(&mut self, value: String) -> Task<Message> {
        self.block_comment_draft = value;
        Task::none()
    }
    fn on_document_transaction(
        &mut self,
        transaction: ::ducktape_view_guest::EditorTransaction<Message>,
    ) -> Task<Message> {
        let route = transaction.apply(&mut self.document);
        route.map_or_else(Task::none, Task::done)
    }
    fn on_document_updated(
        &mut self,
        document: ::ducktape_view_guest::EditorDocumentUpdate,
    ) -> Task<Message> {
        document.apply(&mut self.document);
        Task::none()
    }
}
