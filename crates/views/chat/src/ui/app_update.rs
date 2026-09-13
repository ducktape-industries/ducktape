use super::*;
impl super::ChatView {
    pub(crate) fn update(&mut self, message: Message) -> ducktape_view_guest::Task<Message> {
        match message {
            Message::SidebarResized(dx, _dy) => self.on_sidebar_resized(dx, _dy),
            Message::DetailsResized(dx, _dy) => self.on_details_resized(dx, _dy),
            Message::ThreadResized(dx, _dy) => self.on_thread_resized(dx, _dy),
            Message::ChatViewportChanged(width, _height) => {
                self.on_chat_viewport_changed(width, _height)
            }
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::SessionSettled(moved_room) => self.on_session_settled(moved_room),
            Message::SnapStream(moved) => self.on_snap_stream(moved),
            Message::RevealStream(target_key) => self.on_reveal_stream(target_key),
            Message::RevealThread(target_key) => self.on_reveal_thread(target_key),
            Message::RoomArrived(item) => self.on_room_arrived(item),
            Message::ThreadArrived(item) => self.on_thread_arrived(item),
            Message::SearchArrived(item) => self.on_search_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::SearchChatSubmit => self.on_search_chat_submit(),
            Message::ClearChatSearch => self.on_clear_chat_search(),
            Message::OpenChatSearchHit(channel_id, _root_seq, target_seq) => {
                self.on_open_chat_search_hit(channel_id, _root_seq, target_seq)
            }
            Message::ToggleChannelCreate => self.on_toggle_channel_create(),
            Message::ChooseChannel(id) => self.on_choose_channel(id),
            Message::ChooseDm(peer_key) => self.on_choose_dm(peer_key),
            Message::ToggleChannelSettings => self.on_toggle_channel_settings(),
            Message::ShowHuddle => self.on_show_huddle(),
            Message::LeaveHuddleHere => self.on_leave_huddle_here(),
            Message::JoinHuddleSubmit => self.on_join_huddle_submit(),
            Message::OpenMessageLink(url) => self.on_open_message_link(url),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::CopyMessageLink(link) => self.on_copy_message_link(link),
            Message::CancelRun(run_id) => self.on_cancel_run(run_id),
            Message::OpenRun(dispatch_id) => self.on_open_run(dispatch_id),
            Message::ChatScrolled(absolute_x, absolute_y, relative_x, relative_y) => {
                self.on_chat_scrolled(absolute_x, absolute_y, relative_x, relative_y)
            }
            Message::LoadMoreHistory => self.on_load_more_history(),
            Message::OpenMessageActions(seq, body, rev) => {
                self.on_open_message_actions(seq, body, rev)
            }
            Message::OpenMessageReactions(seq, body, rev) => {
                self.on_open_message_reactions(seq, body, rev)
            }
            Message::BeginMessageEdit(seq, body, rev) => self.on_begin_message_edit(seq, body, rev),
            Message::ArmMessageDelete(seq, body, rev) => self.on_arm_message_delete(seq, body, rev),
            Message::ClearMessageSelection => self.on_clear_message_selection(),
            Message::OpenThreadMessageActions(seq, body, rev) => {
                self.on_open_thread_message_actions(seq, body, rev)
            }
            Message::OpenThreadMessageReactions(seq, body, rev) => {
                self.on_open_thread_message_reactions(seq, body, rev)
            }
            Message::BeginThreadMessageEdit(seq, body, rev) => {
                self.on_begin_thread_message_edit(seq, body, rev)
            }
            Message::ArmThreadMessageDelete(seq, body, rev) => {
                self.on_arm_thread_message_delete(seq, body, rev)
            }
            Message::ClearThreadMessageSelection => self.on_clear_thread_message_selection(),
            Message::OpenThreadFor(seq) => self.on_open_thread_for(seq),
            Message::CloseThread => self.on_close_thread(),
            Message::LoadMoreThread => self.on_load_more_thread(),
            Message::AddReactionSubmit(emoji) => self.on_add_reaction_submit(emoji),
            Message::AddReactionAt(seq, emoji) => self.on_add_reaction_at(seq, emoji),
            Message::RemoveReactionAt(seq, emoji) => self.on_remove_reaction_at(seq, emoji),
            Message::DeleteMessageSubmit => self.on_delete_message_submit(),
            Message::DeleteThreadMessageSubmit => self.on_delete_thread_message_submit(),
            Message::RenameChannelSubmit => self.on_rename_channel_submit(),
            Message::ArchiveChannelSubmit => self.on_archive_channel_submit(),
            Message::UnarchiveChannelSubmit => self.on_unarchive_channel_submit(),
            Message::AddChannelMemberSubmit => self.on_add_channel_member_submit(),
            Message::RemoveChannelMemberSubmit(key) => self.on_remove_channel_member_submit(key),
            Message::PressMessage(seq, surface) => self.on_press_message(seq, surface),
            Message::ClearCopyRange => self.on_clear_copy_range(),
            Message::CopySelectedMessages => self.on_copy_selected_messages(),
            Message::CopyChord(fired) => self.on_copy_chord(fired),
            Message::SearchDraftChanged(value) => self.on_search_draft_changed(value),
            Message::ChannelNameDraftChanged(value) => self.on_channel_name_draft_changed(value),
            Message::MemberKeyDraftChanged(value) => self.on_member_key_draft_changed(value),
        }
    }
    fn on_sidebar_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.sidebar_width = crate::host::sidebar_width_after_delta(
            self.sidebar_width,
            dx,
            self.chat_viewport_width,
        );
        self.details_width = crate::host::details_width_after_delta(
            self.details_width,
            0.0,
            self.chat_viewport_width,
            self.sidebar_width,
        );
        self.thread_width = crate::host::thread_width_after_delta(
            self.thread_width,
            0.0,
            self.chat_viewport_width,
            self.sidebar_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_details_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.details_width = crate::host::details_width_after_delta(
            self.details_width,
            -dx,
            self.chat_viewport_width,
            self.sidebar_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_thread_resized(&mut self, dx: f64, _dy: f64) -> ducktape_view_guest::Task<Message> {
        self.thread_width = crate::host::thread_width_after_delta(
            self.thread_width,
            -dx,
            self.chat_viewport_width,
            self.sidebar_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_chat_viewport_changed(
        &mut self,
        width: f64,
        _height: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.chat_viewport_width = width;
        self.sidebar_width = crate::host::sidebar_width_after_delta(self.sidebar_width, 0.0, width);
        self.details_width = crate::host::details_width_after_delta(
            self.details_width,
            0.0,
            width,
            self.sidebar_width,
        );
        self.thread_width = crate::host::thread_width_after_delta(
            self.thread_width,
            0.0,
            width,
            self.sidebar_width,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_session_arrived(
        &mut self,
        item: crate::host::SessionItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = crate::host::failure_note("Couldn’t read the session", &item.error);
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let next = item.next.clone();
        let sent_now = next.sent_serial != self.sent_serial;
        let chord_now = next.copy_chord_serial != self.copy_chord_serial;
        let moved_room =
            (next.active_channel != self.active_channel) || (next.land_seq != self.land_seq);
        self.sent_serial = next.sent_serial;
        self.copy_chord_serial = next.copy_chord_serial;
        self.connection_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.connection_serial,
        );
        self.connected = next.connected;
        self.dark = next.dark;
        self.endpoint = next.endpoint.to_owned();
        self.network_name = next.network_name.to_owned();
        self.network_chain_id = next.network_chain_id.to_owned();
        self.status = next.status.to_owned();
        self.block_height = next.block_height;
        self.me = next.me.to_owned();
        self.me_key = next.me_key.to_owned();
        self.sent = crate::host::seat_reader(
            ::std::convert::AsRef::as_ref(&(next.me)),
            ::std::convert::AsRef::as_ref(&(next.me_key)),
        );
        self.names_serial = next.names_serial;
        self.rooms = next.rooms.clone();
        self.dm_rows = next.dm_rows.clone();
        self.channel_create_open = next.channel_create_open;
        self.active_channel = next.active_channel.to_owned();
        self.active_dm_peer = next.active_dm_peer.to_owned();
        self.active_dm = next.active_dm.clone();
        self.land_seq = next.land_seq;
        self.unread_boundary = next.unread_boundary;
        self.session_loading = next.loading;
        self.session_busy = next.busy;
        self.busy = self.session_busy;
        self.huddle_joined = next.huddle_joined;
        self.huddle_channel = next.huddle_channel.to_owned();
        self.huddle_channel_name = next.huddle_channel_name.to_owned();
        self.huddle_joined_at = next.huddle_joined_at;
        self.huddle_now = next.huddle_now;
        self.call_muted = next.call_muted;
        self.shift_held = next.shift_held;
        self.pending_sends = next.pending_sends.clone();
        self.live_agents = next.live_agents.clone();
        self.loading = self.session_loading
            || ((!(self.active_channel).is_empty()) && (self.room_channel != self.active_channel));
        ::ducktape_view_guest::Task::batch([
            (::ducktape_view_guest::Task::done(moved_room)).map(Message::SessionSettled),
            (::ducktape_view_guest::Task::done(sent_now)).map(Message::SnapStream),
            (::ducktape_view_guest::Task::done(chord_now)).map(Message::CopyChord),
        ])
    }
    fn on_session_settled(&mut self, moved_room: bool) -> ducktape_view_guest::Task<Message> {
        match crate::host::room_move(moved_room) {
            RoomMove::Stayed => {
                self.messages = crate::host::with_pending(
                    ::std::convert::AsRef::as_ref(&(self.room_messages)),
                    ::std::convert::AsRef::as_ref(&(self.pending_sends)),
                    0,
                    ::std::convert::AsRef::as_ref(&(self.me)),
                );
                self.unread_marker_seq = crate::host::first_unread_seq(
                    ::std::convert::AsRef::as_ref(&(self.messages)),
                    self.unread_boundary,
                );
                self.room_key = crate::host::room_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    self.land_seq,
                    self.history_pages,
                );
                self.thread_key = crate::host::thread_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    self.active_thread_seq,
                    self.thread_target_seq,
                    self.thread_pages,
                );
                ::ducktape_view_guest::Task::none()
            }
            RoomMove::Moved => {
                self.history_pages = 0;
                self.selected_message_seq = 0;
                self.selected_message_rev = 0;
                self.message_action = MessageAction::Toolbar;
                self.message_edit_draft = "".to_owned();
                self.active_thread_seq = 0;
                self.thread_target_seq = 0;
                self.thread_pages = 0;
                {
                    let next = Vec::new();
                    if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                        self.thread_messages = next;
                    }
                }
                self.thread_has_more = false;
                self.thread_next_reply_seq = 0;
                self.thread_loading = false;
                self.thread_selected_seq = 0;
                self.thread_selected_rev = 0;
                self.thread_message_action = MessageAction::Toolbar;
                self.thread_edit_draft = "".to_owned();
                self.copy_anchor_seq = 0;
                self.copy_head_seq = 0;
                self.copy_surface = CopySurface::Nowhere;
                self.channel_settings_open = false;
                self.at_live_tail = true;
                self.room_messages = Vec::new();
                self.messages = Vec::new();
                self.unread_marker_seq = 0;
                self.channel_members = Vec::new();
                self.post_refusal = "".to_owned();
                self.has_older_history = false;
                self.room_key = crate::host::room_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    self.land_seq,
                    0,
                );
                self.thread_key = crate::host::thread_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    0,
                    0,
                    0,
                );
                ::ducktape_view_guest::Task::none()
            }
        }
    }
    fn on_snap_stream(&mut self, moved: bool) -> ducktape_view_guest::Task<Message> {
        if !moved {
            return ::ducktape_view_guest::Task::none();
        }
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Snap {
                target: String::from("ChatView/chat/message-stream"),
                x: 0.0_f32,
                y: 0.0_f32,
            },
        )
    }
    fn on_reveal_stream(&mut self, target_key: i64) -> ducktape_view_guest::Task<Message> {
        if target_key <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::ScrollToKey {
                target: String::from("ChatView/chat/message-stream"),
                key: ::ducktape_view_guest::wire::ListKey::from(target_key).virtual_key(),
            },
        )
    }
    fn on_reveal_thread(&mut self, target_key: i64) -> ducktape_view_guest::Task<Message> {
        if target_key <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::ScrollToKey {
                target: String::from("ChatView/chat/thread-pane/thread-stream"),
                key: ::ducktape_view_guest::wire::ListKey::from(target_key).virtual_key(),
            },
        )
    }
    fn on_room_arrived(
        &mut self,
        item: crate::host::RoomItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = crate::host::failure_note("Couldn’t read this room", &item.error);
        self.history_loading = false;
        if item.channel != self.active_channel {
            return ::ducktape_view_guest::Task::none();
        }
        self.room_channel = item.channel.to_owned();
        self.loading = self.session_loading;
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.active_channel_name = item.name.to_owned();
        self.active_channel_archived = item.archived;
        self.active_channel_members_only = item.members_only;
        self.channel_members = item.members.clone();
        self.post_refusal = crate::host::post_gate(
            item.archived,
            item.members_only,
            ::std::convert::AsRef::as_ref(&(item.members)),
            ::std::convert::AsRef::as_ref(&(self.me)),
        );
        self.room_messages = item.messages.clone();
        self.messages = crate::host::with_pending(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            ::std::convert::AsRef::as_ref(&(self.pending_sends)),
            0,
            ::std::convert::AsRef::as_ref(&(self.me)),
        );
        self.unread_marker_seq = crate::host::first_unread_seq(
            ::std::convert::AsRef::as_ref(&(self.messages)),
            self.unread_boundary,
        );
        self.has_older_history = item.has_older;
        self.history_view = (self.land_seq > 0) || (self.history_pages > 0);
        self.stream_reveal_key = crate::host::message_target_key(
            ::std::convert::AsRef::as_ref(&(self.messages)),
            self.land_seq,
            self.land_seq > 0,
        );
        match crate::host::landing_thread(item.thread_root) {
            LandingThread::Absent => {
                self.thread_key = crate::host::thread_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    self.active_thread_seq,
                    self.thread_target_seq,
                    self.thread_pages,
                );
                (::ducktape_view_guest::Task::done(self.stream_reveal_key))
                    .map(Message::RevealStream)
            }
            LandingThread::Seated => {
                self.active_thread_seq = item.thread_root;
                self.thread_target_seq = self.land_seq;
                self.thread_pages = 0;
                self.thread_loading = true;
                self.thread_key = crate::host::thread_key(
                    self.connection_serial + self.room_serial,
                    self.names_serial,
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    item.thread_root,
                    self.land_seq,
                    0,
                );
                (::ducktape_view_guest::Task::done(self.stream_reveal_key))
                    .map(Message::RevealStream)
            }
        }
    }
    fn on_thread_arrived(
        &mut self,
        item: crate::host::ThreadItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = crate::host::failure_note("Couldn’t read this thread", &item.error);
        self.thread_loading = false;
        if item.root_seq != self.active_thread_seq {
            return ::ducktape_view_guest::Task::none();
        }
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        {
            let next = crate::host::with_pending(
                ::std::convert::AsRef::as_ref(&(item.messages)),
                ::std::convert::AsRef::as_ref(&(self.pending_sends)),
                self.active_thread_seq,
                ::std::convert::AsRef::as_ref(&(self.me)),
            );
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.thread_target_seq = item.target_seq;
        self.thread_has_more = item.has_more;
        self.thread_next_reply_seq = item.next_reply_seq;
        self.thread_reveal_key = crate::host::message_target_key(
            ::std::convert::AsRef::as_ref(&(self.thread_messages)),
            item.target_seq,
            item.target_seq > 0,
        );
        (::ducktape_view_guest::Task::done(self.thread_reveal_key)).map(Message::RevealThread)
    }
    fn on_search_arrived(
        &mut self,
        item: crate::host::SearchItem,
    ) -> ducktape_view_guest::Task<Message> {
        self.host_error = crate::host::failure_note("Search didn’t go through", &item.error);
        if (item.query).is_empty() || (item.query != self.search_query) {
            return ::ducktape_view_guest::Task::none();
        }
        self.search_hits = item.hits.clone();
        match crate::host::search_outcome((item.error).is_empty()) {
            SearchOutcome::Answered => {
                self.search_phase = SearchPhase::Done;
                ::ducktape_view_guest::Task::none()
            }
            SearchOutcome::Refused => {
                self.search_phase = SearchPhase::Idle;
                self.search_query = "".to_owned();
                ::ducktape_view_guest::Task::none()
            }
        }
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ducktape_view_guest::Task<Message> {
        self.busy = self.session_busy;
        self.host_error = crate::host::failure_note("That didn’t go through", &item.error);
        self.selected_message_seq = 0;
        self.selected_message_rev = 0;
        self.message_action = MessageAction::Toolbar;
        self.message_edit_draft = "".to_owned();
        self.thread_selected_seq = 0;
        self.thread_selected_rev = 0;
        self.thread_message_action = MessageAction::Toolbar;
        self.thread_edit_draft = "".to_owned();
        self.member_key_draft = "".to_owned();
        self.room_serial += 1;
        self.room_key = crate::host::room_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.land_seq,
            self.history_pages,
        );
        self.thread_key = crate::host::thread_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.active_thread_seq,
            self.thread_target_seq,
            self.thread_pages,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_search_chat_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((self.search_draft).trim().to_owned()).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.search_phase = SearchPhase::Searching;
        self.search_hits = Vec::new();
        self.search_query = (self.search_draft).trim().to_owned();
        self.host_error = "".to_owned();
        self.search_key = crate::host::search_key(
            self.connection_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.search_query)),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_clear_chat_search(&mut self) -> ducktape_view_guest::Task<Message> {
        self.search_draft = "".to_owned();
        self.search_query = "".to_owned();
        self.search_hits = Vec::new();
        self.search_phase = SearchPhase::Idle;
        self.search_key = crate::host::search_key(
            self.connection_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&("")),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_chat_search_hit(
        &mut self,
        channel_id: String,
        _root_seq: i64,
        target_seq: i64,
    ) -> ducktape_view_guest::Task<Message> {
        self.search_phase = SearchPhase::Idle;
        self.search_hits = Vec::new();
        self.search_query = "".to_owned();
        self.sent =
            crate::host::send_open_hit(::std::convert::AsRef::as_ref(&(channel_id)), target_seq);
        ::ducktape_view_guest::Task::none()
    }
    fn on_toggle_channel_create(&mut self) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_toggle_create();
        ::ducktape_view_guest::Task::none()
    }
    fn on_choose_channel(&mut self, id: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_choose_channel(::std::convert::AsRef::as_ref(&(id)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_choose_dm(&mut self, peer_key: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_choose_dm(::std::convert::AsRef::as_ref(&(peer_key)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_toggle_channel_settings(&mut self) -> ducktape_view_guest::Task<Message> {
        if (self.active_channel).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.channel_name_draft = self.active_channel_name.to_owned();
        self.channel_settings_open = !self.channel_settings_open;
        ::ducktape_view_guest::Task::none()
    }
    fn on_show_huddle(&mut self) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_show_huddle();
        ::ducktape_view_guest::Task::none()
    }
    fn on_leave_huddle_here(&mut self) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_leave_huddle();
        ::ducktape_view_guest::Task::none()
    }
    fn on_join_huddle_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_join_huddle();
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_message_link(&mut self, url: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_open_link(::std::convert::AsRef::as_ref(&(url)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_copy_to_clipboard(
        &mut self,
        text: String,
        label: String,
    ) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_copy(
            ::std::convert::AsRef::as_ref(&(text)),
            ::std::convert::AsRef::as_ref(&(label)),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_copy_message_link(&mut self, link: String) -> ducktape_view_guest::Task<Message> {
        self.message_action = MessageAction::Toolbar;
        self.thread_message_action = MessageAction::Toolbar;
        if (link).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.sent = crate::host::send_copy_link(::std::convert::AsRef::as_ref(&(link)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_cancel_run(&mut self, run_id: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_cancel_run(::std::convert::AsRef::as_ref(&(run_id)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_run(&mut self, dispatch_id: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_open_run(::std::convert::AsRef::as_ref(&(dispatch_id)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_chat_scrolled(
        &mut self,
        absolute_x: f64,
        absolute_y: f64,
        relative_x: f64,
        relative_y: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.at_live_tail = crate::host::near_scroll_tail(relative_y);
        self.sent = crate::host::send_scrolled(absolute_x, absolute_y, relative_x, relative_y);
        if (((((!crate::host::near_scroll_top(relative_y)) || self.history_loading)
            || self.loading)
            || self.busy)
            || (self.active_channel).is_empty())
            || (!self.has_older_history)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.history_loading = true;
        self.history_pages += 1;
        self.room_key = crate::host::room_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.land_seq,
            self.history_pages,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_load_more_history(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((((self.history_loading || self.loading) || self.busy)
            || (self.active_channel).is_empty())
            || (self.messages).is_empty())
            || (!self.has_older_history)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.history_loading = true;
        self.history_pages += 1;
        self.room_key = crate::host::room_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.land_seq,
            self.history_pages,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_message_actions(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.selected_message_seq = seq;
        self.selected_message_rev = rev;
        self.message_action = MessageAction::More;
        self.message_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/message-action-focus"),
            },
        )
    }
    fn on_open_message_reactions(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = crate::host::reaction_refusal(
            self.active_channel_archived,
            ::std::convert::AsRef::as_ref(&(self.host_error)),
        );
        if self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.selected_message_seq = seq;
        self.selected_message_rev = rev;
        self.message_action = MessageAction::Reactions;
        self.message_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/message-reaction-focus"),
            },
        )
    }
    fn on_begin_message_edit(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        let seed =
            crate::host::edit_body_of(::std::convert::AsRef::as_ref(&(self.messages)), seq, rev);
        if (seed).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.sent = crate::host::send_begin_edit(
            ::std::convert::AsRef::as_ref(
                &(crate::host::edit_scope(
                    ::std::convert::AsRef::as_ref(&(self.endpoint)),
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    seq,
                )),
            ),
            ::std::convert::AsRef::as_ref(&(seed)),
            seq,
            rev,
        );
        self.selected_message_seq = seq;
        self.selected_message_rev = rev;
        self.message_action = MessageAction::Editing;
        self.message_edit_draft = body.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_arm_message_delete(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.selected_message_seq = seq;
        self.selected_message_rev = rev;
        self.message_action = MessageAction::Delete;
        self.message_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/message-delete-focus"),
            },
        )
    }
    fn on_clear_message_selection(&mut self) -> ducktape_view_guest::Task<Message> {
        self.selected_message_seq = 0;
        self.selected_message_rev = 0;
        self.message_action = MessageAction::Toolbar;
        self.message_edit_draft = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_thread_message_actions(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.thread_selected_seq = seq;
        self.thread_selected_rev = rev;
        self.thread_message_action = MessageAction::More;
        self.thread_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/thread-pane/thread-action-focus"),
            },
        )
    }
    fn on_open_thread_message_reactions(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = crate::host::reaction_refusal(
            self.active_channel_archived,
            ::std::convert::AsRef::as_ref(&(self.host_error)),
        );
        if self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.thread_selected_seq = seq;
        self.thread_selected_rev = rev;
        self.thread_message_action = MessageAction::Reactions;
        self.thread_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/thread-pane/thread-reaction-focus"),
            },
        )
    }
    fn on_begin_thread_message_edit(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        let seed = crate::host::edit_body_of(
            ::std::convert::AsRef::as_ref(&(self.thread_messages)),
            seq,
            rev,
        );
        if (seed).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.sent = crate::host::send_begin_edit(
            ::std::convert::AsRef::as_ref(
                &(crate::host::edit_scope(
                    ::std::convert::AsRef::as_ref(&(self.endpoint)),
                    ::std::convert::AsRef::as_ref(&(self.active_channel)),
                    seq,
                )),
            ),
            ::std::convert::AsRef::as_ref(&(seed)),
            seq,
            rev,
        );
        self.thread_selected_seq = seq;
        self.thread_selected_rev = rev;
        self.thread_message_action = MessageAction::Editing;
        self.thread_edit_draft = body.to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_arm_thread_message_delete(
        &mut self,
        seq: i64,
        body: String,
        rev: i64,
    ) -> ducktape_view_guest::Task<Message> {
        if seq <= 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.thread_selected_seq = seq;
        self.thread_selected_rev = rev;
        self.thread_message_action = MessageAction::Delete;
        self.thread_edit_draft = body.to_owned();
        ::ducktape_view_guest::widget::perform::<Message>(
            ::ducktape_view_guest::wire::WidgetCommand::Focus {
                target: String::from("ChatView/chat/thread-pane/thread-delete-focus"),
            },
        )
    }
    fn on_clear_thread_message_selection(&mut self) -> ducktape_view_guest::Task<Message> {
        self.thread_selected_seq = 0;
        self.thread_selected_rev = 0;
        self.thread_message_action = MessageAction::Toolbar;
        self.thread_edit_draft = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_open_thread_for(&mut self, seq: i64) -> ducktape_view_guest::Task<Message> {
        if (seq <= 0) || (self.active_channel).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.channel_settings_open = false;
        self.selected_message_seq = 0;
        self.selected_message_rev = 0;
        self.message_action = MessageAction::Toolbar;
        self.message_edit_draft = "".to_owned();
        self.thread_selected_seq = 0;
        self.thread_selected_rev = 0;
        self.thread_message_action = MessageAction::Toolbar;
        self.thread_edit_draft = "".to_owned();
        self.copy_anchor_seq = 0;
        self.copy_head_seq = 0;
        self.copy_surface = CopySurface::Nowhere;
        self.thread_loading = true;
        {
            let next = Vec::new();
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.thread_has_more = false;
        self.thread_next_reply_seq = 0;
        self.thread_pages = 0;
        self.active_thread_seq = seq;
        self.thread_target_seq = 0;
        self.thread_key = crate::host::thread_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            seq,
            0,
            0,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_close_thread(&mut self) -> ducktape_view_guest::Task<Message> {
        self.active_thread_seq = 0;
        self.thread_target_seq = 0;
        {
            let next = Vec::new();
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.thread_pages = 0;
        self.thread_has_more = false;
        self.thread_next_reply_seq = 0;
        self.thread_loading = false;
        self.thread_selected_seq = 0;
        self.thread_selected_rev = 0;
        self.thread_message_action = MessageAction::Toolbar;
        self.thread_edit_draft = "".to_owned();
        self.copy_anchor_seq = 0;
        self.copy_head_seq = 0;
        self.copy_surface = CopySurface::Nowhere;
        self.thread_key = crate::host::thread_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            0,
            0,
            0,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_load_more_thread(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((self.thread_loading || self.busy) || (self.active_thread_seq <= 0))
            || (!self.thread_has_more)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.thread_loading = true;
        self.thread_pages += 1;
        self.thread_key = crate::host::thread_key(
            self.connection_serial + self.room_serial,
            self.names_serial,
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.active_thread_seq,
            self.thread_target_seq,
            self.thread_pages,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_add_reaction_submit(&mut self, emoji: String) -> ducktape_view_guest::Task<Message> {
        if (self.active_channel).is_empty() || (self.selected_message_seq <= 0) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = crate::host::reaction_refusal(
            self.active_channel_archived,
            ::std::convert::AsRef::as_ref(&(self.host_error)),
        );
        if self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.room_messages = crate::host::reaction_applied(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            self.selected_message_seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            true,
        );
        self.messages = crate::host::with_pending(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            ::std::convert::AsRef::as_ref(&(self.pending_sends)),
            0,
            ::std::convert::AsRef::as_ref(&(self.me)),
        );
        {
            let next = crate::host::reaction_applied(
                ::std::convert::AsRef::as_ref(&(self.thread_messages)),
                self.selected_message_seq,
                ::std::convert::AsRef::as_ref(&(emoji)),
                true,
            );
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.sent = crate::host::write_reaction(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.selected_message_seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            true,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_add_reaction_at(
        &mut self,
        seq: i64,
        emoji: String,
    ) -> ducktape_view_guest::Task<Message> {
        if (self.active_channel).is_empty() || (seq <= 0) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = crate::host::reaction_refusal(
            self.active_channel_archived,
            ::std::convert::AsRef::as_ref(&(self.host_error)),
        );
        if self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.room_messages = crate::host::reaction_applied(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            true,
        );
        self.messages = crate::host::with_pending(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            ::std::convert::AsRef::as_ref(&(self.pending_sends)),
            0,
            ::std::convert::AsRef::as_ref(&(self.me)),
        );
        {
            let next = crate::host::reaction_applied(
                ::std::convert::AsRef::as_ref(&(self.thread_messages)),
                seq,
                ::std::convert::AsRef::as_ref(&(emoji)),
                true,
            );
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.sent = crate::host::write_reaction(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            true,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_remove_reaction_at(
        &mut self,
        seq: i64,
        emoji: String,
    ) -> ducktape_view_guest::Task<Message> {
        if (self.active_channel).is_empty() || (seq <= 0) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = crate::host::reaction_refusal(
            self.active_channel_archived,
            ::std::convert::AsRef::as_ref(&(self.host_error)),
        );
        if self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.room_messages = crate::host::reaction_applied(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            false,
        );
        self.messages = crate::host::with_pending(
            ::std::convert::AsRef::as_ref(&(self.room_messages)),
            ::std::convert::AsRef::as_ref(&(self.pending_sends)),
            0,
            ::std::convert::AsRef::as_ref(&(self.me)),
        );
        {
            let next = crate::host::reaction_applied(
                ::std::convert::AsRef::as_ref(&(self.thread_messages)),
                seq,
                ::std::convert::AsRef::as_ref(&(emoji)),
                false,
            );
            if ::ducktape_view_guest::state_changed!(self.thread_messages, next) {
                self.thread_messages = next;
            }
        }
        self.sent = crate::host::write_reaction(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            seq,
            ::std::convert::AsRef::as_ref(&(emoji)),
            false,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_delete_message_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((self.busy || (self.active_channel).is_empty()) || (self.selected_message_seq <= 0))
            || (self.message_action != MessageAction::Delete)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_delete(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.selected_message_seq,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_delete_thread_message_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if ((self.busy || (self.active_channel).is_empty()) || (self.thread_selected_seq <= 0))
            || (self.thread_message_action != MessageAction::Delete)
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_delete(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            self.thread_selected_seq,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_rename_channel_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (self.busy || (self.active_channel).is_empty())
            || ((self.channel_name_draft).trim().to_owned()).is_empty()
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_rename(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            ::std::convert::AsRef::as_ref(&((self.channel_name_draft).trim().to_owned())),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_archive_channel_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (self.busy || (self.active_channel).is_empty()) || self.active_channel_archived {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_archived(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            true,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_unarchive_channel_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (self.busy || (self.active_channel).is_empty()) || (!self.active_channel_archived) {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_archived(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            false,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_add_channel_member_submit(&mut self) -> ducktape_view_guest::Task<Message> {
        if (self.busy || (self.active_channel).is_empty())
            || ((self.member_key_draft).trim().to_owned()).is_empty()
        {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_membership(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            ::std::convert::AsRef::as_ref(&((self.member_key_draft).trim().to_owned())),
            true,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_remove_channel_member_submit(
        &mut self,
        key: String,
    ) -> ducktape_view_guest::Task<Message> {
        if (self.busy || (self.active_channel).is_empty()) || (key).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        self.host_error = "".to_owned();
        self.busy = crate::host::write_membership(
            ::std::convert::AsRef::as_ref(&(self.active_channel)),
            ::std::convert::AsRef::as_ref(&(key)),
            false,
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_press_message(
        &mut self,
        seq: i64,
        surface: CopySurface,
    ) -> ducktape_view_guest::Task<Message> {
        if !self.shift_held {
            return ::ducktape_view_guest::Task::none();
        }
        let range = crate::host::copy_range_after_press(
            self.copy_anchor_seq,
            self.copy_surface,
            seq,
            surface,
        );
        self.copy_anchor_seq = range.anchor;
        self.copy_head_seq = range.head;
        self.copy_surface =
            crate::host::copy_surface_of(::std::convert::AsRef::as_ref(&(range.surface)));
        ::ducktape_view_guest::Task::none()
    }
    fn on_clear_copy_range(&mut self) -> ducktape_view_guest::Task<Message> {
        self.copy_anchor_seq = 0;
        self.copy_head_seq = 0;
        self.copy_surface = CopySurface::Nowhere;
        ::ducktape_view_guest::Task::none()
    }
    fn on_copy_selected_messages(&mut self) -> ducktape_view_guest::Task<Message> {
        let rows = crate::host::copy_range_rows(
            ::std::convert::AsRef::as_ref(&(self.messages)),
            ::std::convert::AsRef::as_ref(&(self.thread_messages)),
            self.copy_surface,
        );
        let count = crate::host::copy_range_count(
            ::std::convert::AsRef::as_ref(&(rows)),
            self.copy_anchor_seq,
            self.copy_head_seq,
        );
        if count == 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.sent = crate::host::send_copy(
            ::std::convert::AsRef::as_ref(
                &(crate::host::copy_range_text(
                    ::std::convert::AsRef::as_ref(&(rows)),
                    self.copy_anchor_seq,
                    self.copy_head_seq,
                )),
            ),
            ::std::convert::AsRef::as_ref(&(crate::host::copy_range_label(count))),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_copy_chord(&mut self, fired: bool) -> ducktape_view_guest::Task<Message> {
        if !fired {
            return ::ducktape_view_guest::Task::none();
        }
        let rows = crate::host::copy_range_rows(
            ::std::convert::AsRef::as_ref(&(self.messages)),
            ::std::convert::AsRef::as_ref(&(self.thread_messages)),
            self.copy_surface,
        );
        let count = crate::host::copy_range_count(
            ::std::convert::AsRef::as_ref(&(rows)),
            self.copy_anchor_seq,
            self.copy_head_seq,
        );
        if count == 0 {
            return ::ducktape_view_guest::Task::none();
        }
        self.sent = crate::host::send_copy(
            ::std::convert::AsRef::as_ref(
                &(crate::host::copy_range_text(
                    ::std::convert::AsRef::as_ref(&(rows)),
                    self.copy_anchor_seq,
                    self.copy_head_seq,
                )),
            ),
            ::std::convert::AsRef::as_ref(&(crate::host::copy_range_label(count))),
        );
        ::ducktape_view_guest::Task::none()
    }
    fn on_search_draft_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.search_draft = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_channel_name_draft_changed(
        &mut self,
        value: String,
    ) -> ducktape_view_guest::Task<Message> {
        self.channel_name_draft = value;
        ::ducktape_view_guest::Task::none()
    }
    fn on_member_key_draft_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        self.member_key_draft = value;
        ::ducktape_view_guest::Task::none()
    }
}
