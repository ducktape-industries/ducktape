use super::*;
impl super::ChatView {
    pub(crate) fn update(&mut self, message: Message) -> ducktape_view_guest::Task<Message> {
        match message {
            Message::Composer(message) => self.on_composer(*message),
            Message::SidebarResized(dx, _dy) => self.on_sidebar_resized(dx, _dy),
            Message::DetailsResized(dx, _dy) => self.on_details_resized(dx, _dy),
            Message::ThreadResized(dx, _dy) => self.on_thread_resized(dx, _dy),
            Message::ChatViewportChanged(width, height) => {
                self.on_chat_viewport_changed(width, height)
            }
            Message::PressedAt(x, y) => self.on_pressed_at(x, y),
            Message::PictureLoaded(link, drawn) => self.on_picture_loaded(link, drawn),
            Message::OpenAttachment(link) => self.on_open_attachment(link),
            Message::ClosePreview => self.on_close_preview(),
            Message::PreviewArrived(item) => self.on_preview_arrived(item),
            Message::SessionArrived(item) => self.on_session_arrived(*item),
            Message::SidebarArrived(item) => self.on_sidebar_arrived(item),
            Message::VisibilityChanged(visible) => self.on_visibility_changed(visible),
            Message::SessionSettled(moved_room) => self.on_session_settled(moved_room),
            Message::BackgroundFinished => self.on_background_finished(),
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
            Message::ChannelDraftChanged(value) => self.on_channel_draft_changed(value),
            Message::ToggleChannelVoice => self.on_toggle_channel_voice(),
            Message::ToggleChannelMembersOnly => self.on_toggle_channel_members_only(),
            Message::CreateChannel => self.on_create_channel(),
            Message::ChannelIdReady(generation, result) => {
                self.on_channel_id_ready(generation, result)
            }
            Message::ChannelCreated(generation, result) => {
                self.on_channel_created(generation, result)
            }
            Message::ChooseChannel(id) => self.on_choose_channel(id),
            Message::ChooseDm(peer_key) => self.on_choose_dm(peer_key),
            Message::DmOpened(generation, result) => self.on_dm_opened(generation, result),
            Message::ToggleChannelSettings => self.on_toggle_channel_settings(),
            Message::ShowHuddle => self.on_show_huddle(),
            Message::LeaveHuddleHere => self.on_leave_huddle_here(),
            Message::JoinHuddleSubmit => self.on_join_huddle_submit(),
            Message::JoinVoice(id) => self.on_join_voice(id),
            Message::OpenMessageLink(url) => self.on_open_message_link(url),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::CopyMessageLink(link) => self.on_copy_message_link(link),
            Message::CancelRun(run_id) => self.on_cancel_run(run_id),
            Message::RunCancelled(connection, item) => self.on_run_cancelled(connection, item),
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
    /// The last press, in chat-screen pixels; it fires before the button it
    /// landed on, so a menu that opens next knows where.
    fn on_pressed_at(&mut self, x: f64, y: f64) -> ducktape_view_guest::Task<Message> {
        self.press_x = x;
        self.press_y = y;
        ::ducktape_view_guest::Task::none()
    }
    /// Every picture attachment on screen the host has not been asked for
    /// yet: one `picture.load` each, answered as `PictureLoaded`.
    fn picture_tasks(&mut self) -> ducktape_view_guest::Task<Message> {
        let links = crate::host::picture_links(&self.messages, &self.thread_messages);
        let mut tasks = Vec::new();
        for link in links {
            let known = self.pictures.contains_key(&link) || self.pictures_pending.contains(&link);
            if known {
                continue;
            }
            self.pictures_pending.insert(link.clone());
            let path = crate::host::attachment_file_path(&link);
            tasks.push(::ducktape_view_guest::Task::perform(
                crate::host::picture_load(path),
                move |drawn| Message::PictureLoaded(link.clone(), drawn),
            ));
        }
        ::ducktape_view_guest::Task::batch(tasks)
    }
    /// A picture that did not decode stays a file card: (0, 0) says so and
    /// keeps it from being asked for again.
    fn on_picture_loaded(
        &mut self,
        link: String,
        drawn: Result<(i64, i64), String>,
    ) -> ducktape_view_guest::Task<Message> {
        self.pictures_pending.remove(&link);
        self.pictures.insert(link, drawn.unwrap_or((0, 0)));
        ::ducktape_view_guest::Task::none()
    }
    /// A menu opens where the pointer pressed and stays there: a press on
    /// one of its items must not move it out from under the release.
    fn anchor_menu(&mut self) {
        self.menu_x = self.press_x;
        self.menu_y = self.press_y;
    }
    fn on_chat_viewport_changed(
        &mut self,
        width: f64,
        height: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.chat_viewport_width = width;
        self.chat_viewport_height = height;
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
        if let Some(intent) = item.background {
            return ducktape_view_guest::Task::perform(crate::host::run_background(intent), |_| {
                Message::BackgroundFinished
            });
        }
        self.host_error = crate::host::failure_note("Couldn’t read the session", &item.error);
        if !(item.error).is_empty() {
            return ::ducktape_view_guest::Task::none();
        }
        let next = item.next.clone();
        let chord_now = next.copy_chord_serial != self.copy_chord_serial;
        let moved_room =
            (next.active_channel != self.active_channel) || (next.land_seq != self.land_seq);
        self.copy_chord_serial = next.copy_chord_serial;
        let changed_reader = self.endpoint != next.endpoint
            || self.network_chain_id != next.network_chain_id
            || self.me_key != next.me_key;
        let requested_dm =
            next.connected && !next.dm_peer.is_empty() && next.dm_serial != self.dm_request_serial;
        let changed_dm_reader = changed_reader || self.me != next.me;
        let cancel_creation = changed_dm_reader || !next.connected || moved_room || requested_dm;
        if cancel_creation {
            self.retire_channel_creation();
            self.channel_create_open = false;
            self.channel_create_id.clear();
        }
        let cancel_dm = changed_dm_reader || !next.connected || moved_room || requested_dm;
        if cancel_dm {
            self.retire_dm();
        }
        if next.connected || next.dm_peer.is_empty() {
            self.dm_request_serial = next.dm_serial;
        }
        let rebound = changed_reader || (!self.connected && next.connected);
        if rebound {
            self.upload_handles.clear();
            for draft in self.composers.values_mut() {
                draft.retire_device_requests();
            }
            self.sending.clear();
        }
        if changed_reader {
            self.rooms.clear();
            self.dm_rows.clear();
            self.read_cursors.clear();
            crate::host::reset_directory();
            self.connection_serial += 1;
        }
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
        let changed_channel = self.active_channel != next.active_channel;
        let changed_room_identity = changed_reader || changed_channel;
        if changed_room_identity {
            self.unread_boundary = 0;
            if self.read_visit != ReadVisit::Hidden {
                self.read_visit = ReadVisit::Entering;
            }
        }
        self.active_channel = next.active_channel.to_owned();
        self.refresh_active_dm();
        self.land_seq = next.land_seq;
        self.session_loading = next.loading;
        self.session_busy = next.busy;
        self.busy = self.session_busy;
        self.huddle_joined = next.huddle_joined;
        self.huddle_channel = next.huddle_channel.to_owned();
        self.huddle_channel_name = next.huddle_channel_name.to_owned();
        self.huddle_joined_at = next.huddle_joined_at;
        self.huddle_now = next.huddle_now;
        self.call_muted = next.call_muted;
        self.call_speaking = next.call_speaking;
        self.call_peers = next.call_peers.clone();
        self.shift_held = next.shift_held;
        self.refresh_pending();
        self.live_agents = next
            .live_agents
            .iter()
            .filter(|run| run.channel_id == self.active_channel)
            .cloned()
            .map(crate::live::project)
            .collect();
        self.live_agents.sort_by_key(|run| run.anchor_seq);
        self.loading = self.session_loading
            || self.dm_opening.is_some()
            || ((!(self.active_channel).is_empty()) && (self.room_channel != self.active_channel));
        ::ducktape_view_guest::Task::batch([
            if requested_dm {
                ducktape_view_guest::Task::done(Message::ChooseDm(next.dm_peer))
            } else {
                ducktape_view_guest::Task::none()
            },
            (::ducktape_view_guest::Task::done(moved_room)).map(Message::SessionSettled),
            (::ducktape_view_guest::Task::done(chord_now)).map(Message::CopyChord),
        ])
    }
    fn on_visibility_changed(&mut self, visible: bool) -> ducktape_view_guest::Task<Message> {
        if !visible {
            self.retire_dm();
            self.retire_channel_creation();
            self.channel_create_open = false;
        }
        let was_visible = self.read_visit != ReadVisit::Hidden;
        if was_visible == visible {
            return ducktape_view_guest::Task::none();
        }
        self.read_visit = if visible {
            ReadVisit::Entering
        } else {
            ReadVisit::Hidden
        };
        let reads_current_view = visible && self.connected;
        if !reads_current_view {
            return ducktape_view_guest::Task::none();
        }
        ducktape_view_guest::Task::perform(
            crate::host::read_sidebar(self.connection_serial, self.names_serial, self.me.clone()),
            Message::SidebarArrived,
        )
    }

    fn on_sidebar_arrived(
        &mut self,
        item: crate::host::SidebarItem,
    ) -> ducktape_view_guest::Task<Message> {
        let current_directory = item.connection_serial == self.connection_serial
            && item.names_serial == self.names_serial;
        if !current_directory {
            return ducktape_view_guest::Task::none();
        }
        if !item.error.is_empty() {
            self.host_error = crate::host::failure_note("Couldn’t read the sidebar", &item.error);
            return ducktape_view_guest::Task::none();
        }
        let mut heads = std::collections::BTreeMap::new();
        for channel in &item.channels {
            let cursor = self
                .read_cursors
                .entry(channel.id.clone())
                .or_insert(channel.head_seq);
            let reading_live_room = self.read_visit != ReadVisit::Hidden
                && self.land_seq == 0
                && self.history_pages == 0
                && channel.id == self.active_channel;
            if reading_live_room {
                let arrived_with_unread =
                    self.read_visit == ReadVisit::Entering && channel.head_seq > *cursor;
                if arrived_with_unread {
                    self.unread_boundary = *cursor;
                }
                self.read_visit = ReadVisit::Reading;
                *cursor = (*cursor).max(channel.head_seq);
            }
            heads.insert(channel.id.clone(), channel.head_seq > *cursor);
        }
        self.dm_rows = item
            .peers
            .into_iter()
            .map(|peer| crate::host::DmSidebarRow {
                unread: heads.get(&peer.channel_id).copied().unwrap_or(false),
                peer,
            })
            .collect();
        self.rooms = item
            .channels
            .into_iter()
            .filter(|channel| {
                !channel.id.strip_prefix("dm-").is_some_and(|suffix| {
                    suffix.len() == 64
                        && suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            })
            .map(|channel| crate::host::ChatSidebarRow {
                unread: heads.get(&channel.id).copied().unwrap_or(false),
                channel,
            })
            .collect();
        self.refresh_active_dm();
        self.unread_marker_seq =
            crate::host::first_unread_seq(&self.messages, self.unread_boundary);
        ducktape_view_guest::Task::none()
    }

    fn refresh_active_dm(&mut self) {
        self.active_dm = self
            .dm_rows
            .iter()
            .find(|row| {
                !row.peer.channel_id.is_empty() && row.peer.channel_id == self.active_channel
            })
            .map(|row| row.peer.clone())
            .unwrap_or_default();
        self.active_dm_peer = self.active_dm.key.clone();
    }

    fn on_background_finished(&mut self) -> ducktape_view_guest::Task<Message> {
        ducktape_view_guest::Task::none()
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
        self.loading = self.session_loading || self.dm_opening.is_some();
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
        self.sending
            .retain(|id, _| !item.messages.iter().any(|row| &row.id == id));
        self.refresh_pending();
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
        self.window_reaches_head = item.reaches_head;
        self.history_view = (self.land_seq > 0) || (self.history_pages > 0);
        self.stream_reveal_key = crate::host::message_target_key(
            ::std::convert::AsRef::as_ref(&(self.messages)),
            self.land_seq,
            self.land_seq > 0,
        );
        let pictures = self.picture_tasks();
        let landed = match crate::host::landing_thread(item.thread_root) {
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
        };
        ::ducktape_view_guest::Task::batch([landed, pictures])
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
        self.sending
            .retain(|id, _| !item.messages.iter().any(|row| &row.id == id));
        self.refresh_pending();
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
        ::ducktape_view_guest::Task::batch([
            (::ducktape_view_guest::Task::done(self.thread_reveal_key)).map(Message::RevealThread),
            self.picture_tasks(),
        ])
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
        self.retire_dm();
        self.retire_channel_creation();
        self.channel_create_open = false;
        self.search_phase = SearchPhase::Idle;
        self.search_hits = Vec::new();
        self.search_query = "".to_owned();
        self.sent = crate::host::send_open_link(&crate::host::duck_channel_message_link(
            channel_id,
            target_seq,
            self.network_chain_id.clone(),
        ));
        ::ducktape_view_guest::Task::none()
    }
    fn retire_channel_creation(&mut self) {
        self.channel_creating.take();
        self.channel_create_generation = self.channel_create_generation.wrapping_add(1);
    }
    fn on_toggle_channel_create(&mut self) -> ducktape_view_guest::Task<Message> {
        if self.channel_creating.is_some() {
            return ducktape_view_guest::Task::none();
        }
        self.channel_create_open = !self.channel_create_open;
        self.channel_create_error.clear();
        ducktape_view_guest::Task::none()
    }
    fn on_channel_draft_changed(&mut self, value: String) -> ducktape_view_guest::Task<Message> {
        if self.channel_creating.is_none() {
            let changed = self.channel_draft != value;
            if changed {
                self.channel_create_id.clear();
            }
            self.channel_draft = value;
        }
        ducktape_view_guest::Task::none()
    }
    fn on_toggle_channel_voice(&mut self) -> ducktape_view_guest::Task<Message> {
        if self.channel_creating.is_none() {
            self.channel_create_id.clear();
            self.channel_create_voice = !self.channel_create_voice;
        }
        ducktape_view_guest::Task::none()
    }
    fn on_toggle_channel_members_only(&mut self) -> ducktape_view_guest::Task<Message> {
        let may_toggle = self.channel_creating.is_none() && !self.channel_create_voice;
        if may_toggle {
            self.channel_create_id.clear();
            self.channel_create_members_only = !self.channel_create_members_only;
        }
        ducktape_view_guest::Task::none()
    }
    fn on_create_channel(&mut self) -> ducktape_view_guest::Task<Message> {
        let may_create = self.connected && !self.session_busy && self.channel_creating.is_none();
        if !may_create {
            return ducktape_view_guest::Task::none();
        }
        let name = self.channel_draft.trim();
        let invalid_name = name.is_empty() || name.len() > 128 || name.contains('\0');
        if invalid_name {
            self.channel_create_error = "Enter a channel name of at most 128 bytes".into();
            return ducktape_view_guest::Task::none();
        }
        self.channel_create_error.clear();
        self.retire_channel_creation();
        let generation = self.channel_create_generation;
        if !self.channel_create_id.is_empty() {
            return self.on_channel_id_ready(generation, Ok(self.channel_create_id.clone()));
        }
        let (task, handle) =
            ducktape_view_guest::Task::perform(crate::host::mint_channel(), move |result| {
                Message::ChannelIdReady(generation, result)
            })
            .abortable();
        self.channel_creating = Some(handle.abort_on_drop());
        task
    }
    fn on_channel_id_ready(
        &mut self,
        generation: u64,
        result: Result<String, String>,
    ) -> ducktape_view_guest::Task<Message> {
        if generation != self.channel_create_generation {
            return ducktape_view_guest::Task::none();
        }
        self.channel_creating.take();
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                self.channel_create_error = error;
                return ducktape_view_guest::Task::none();
            }
        };
        self.channel_create_id = id.clone();
        let (task, handle) = ducktape_view_guest::Task::perform(
            crate::host::create_channel(
                id,
                self.channel_draft.trim().into(),
                self.channel_create_voice,
                self.channel_create_members_only,
            ),
            move |result| Message::ChannelCreated(generation, result),
        )
        .abortable();
        self.channel_creating = Some(handle.abort_on_drop());
        task
    }
    fn on_channel_created(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> ducktape_view_guest::Task<Message> {
        if generation != self.channel_create_generation {
            return ducktape_view_guest::Task::none();
        }
        self.channel_creating.take();
        if let Err(error) = result {
            self.channel_create_error =
                crate::host::failure_note("Couldn’t create this channel", &error);
            return ducktape_view_guest::Task::none();
        }
        if !self.channel_create_voice {
            self.sent =
                crate::host::send_open_link(&format!("duck://channel/{}", self.channel_create_id));
        }
        self.channel_create_open = false;
        self.channel_draft.clear();
        self.channel_create_id.clear();
        self.channel_create_voice = false;
        self.channel_create_members_only = false;
        ducktape_view_guest::Task::perform(
            crate::host::read_sidebar(self.connection_serial, self.names_serial, self.me.clone()),
            Message::SidebarArrived,
        )
    }
    fn on_choose_channel(&mut self, id: String) -> ducktape_view_guest::Task<Message> {
        self.retire_dm();
        self.retire_channel_creation();
        self.channel_create_open = false;
        self.sent = crate::host::send_open_link(&crate::host::duck_channel_link(
            id,
            self.network_chain_id.clone(),
        ));
        ::ducktape_view_guest::Task::none()
    }
    fn retire_dm(&mut self) {
        self.dm_opening.take();
        self.dm_generation = self.dm_generation.wrapping_add(1);
        self.loading = self.session_loading || self.room_channel != self.active_channel;
    }
    fn on_choose_dm(&mut self, peer: String) -> ducktape_view_guest::Task<Message> {
        let may_open = self.connected && !self.session_busy && !peer.is_empty();
        if !may_open {
            return ducktape_view_guest::Task::none();
        }
        self.retire_channel_creation();
        self.channel_create_open = false;
        self.retire_dm();
        let generation = self.dm_generation;
        let (task, handle) = ducktape_view_guest::Task::perform(
            crate::host::open_dm(self.me.clone(), peer),
            move |result| Message::DmOpened(generation, result),
        )
        .abortable();
        self.dm_opening = Some(handle.abort_on_drop());
        self.loading = true;
        self.host_error.clear();
        task
    }
    fn on_dm_opened(
        &mut self,
        generation: u64,
        result: Result<String, String>,
    ) -> ducktape_view_guest::Task<Message> {
        if generation != self.dm_generation {
            return ducktape_view_guest::Task::none();
        }
        self.dm_opening.take();
        self.loading = self.session_loading || self.room_channel != self.active_channel;
        match result {
            Ok(channel) => {
                self.sent = crate::host::send_open_link(&format!("duck://channel/{channel}"));
            }
            Err(error) => {
                self.host_error =
                    crate::host::failure_note("Couldn’t open this conversation", &error);
            }
        }
        ducktape_view_guest::Task::none()
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
    fn on_join_voice(&mut self, id: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_join_voice(&id);
        ::ducktape_view_guest::Task::none()
    }
    /// A link leaves for the app's own router; a preview card gives way to
    /// the tab it lands on.
    fn on_open_message_link(&mut self, url: String) -> ducktape_view_guest::Task<Message> {
        self.retire_dm();
        self.retire_channel_creation();
        self.channel_create_open = false;
        self.preview_link = "".to_owned();
        self.sent = crate::host::send_open_link(::std::convert::AsRef::as_ref(&(url)));
        ::ducktape_view_guest::Task::none()
    }
    /// The preview card opens over the screen on the file pressed. A
    /// picture is already decoded for its thumbnail; anything else is read
    /// by the subscription this serial keys.
    fn on_open_attachment(&mut self, link: String) -> ducktape_view_guest::Task<Message> {
        self.preview_link = link;
        self.preview_serial += 1;
        self.preview = crate::host::PreviewItem::default();
        ::ducktape_view_guest::Task::none()
    }
    fn on_close_preview(&mut self) -> ducktape_view_guest::Task<Message> {
        self.preview_link = "".to_owned();
        ::ducktape_view_guest::Task::none()
    }
    fn on_preview_arrived(
        &mut self,
        item: crate::host::PreviewItem,
    ) -> ducktape_view_guest::Task<Message> {
        let current = item.path == crate::host::attachment_file_path(&self.preview_link);
        if current {
            self.preview = item;
        }
        ::ducktape_view_guest::Task::none()
    }
    /// Does the open preview read its file? A picture draws from the host's
    /// slot instead.
    pub(crate) fn preview_reads(&self) -> bool {
        let open = !self.preview_link.is_empty();
        let picture =
            matches!(self.pictures.get(&self.preview_link), Some(&(w, h)) if w > 0 && h > 0);
        open && !picture
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
        self.sent = crate::host::send_copy(&link, "Message link copied");
        ::ducktape_view_guest::Task::none()
    }
    fn on_cancel_run(&mut self, run_id: String) -> ducktape_view_guest::Task<Message> {
        let connection = self.connection_serial;
        ducktape_view_guest::Task::perform(crate::host::cancel_run(run_id), move |item| {
            Message::RunCancelled(connection, item)
        })
    }
    fn on_run_cancelled(
        &mut self,
        connection: i64,
        item: crate::host::ActItem,
    ) -> ducktape_view_guest::Task<Message> {
        let current_connection = connection == self.connection_serial;
        if current_connection {
            self.host_error = crate::host::failure_note("Couldn’t stop the run", &item.error);
        }
        ducktape_view_guest::Task::none()
    }
    fn on_open_run(&mut self, dispatch_id: String) -> ducktape_view_guest::Task<Message> {
        self.sent = crate::host::send_open_link(&crate::host::duck_run_link(
            dispatch_id,
            self.network_chain_id.clone(),
        ));
        ::ducktape_view_guest::Task::none()
    }
    fn on_chat_scrolled(
        &mut self,
        _absolute_x: f64,
        _absolute_y: f64,
        _relative_x: f64,
        relative_y: f64,
    ) -> ducktape_view_guest::Task<Message> {
        self.at_live_tail = crate::host::near_scroll_tail(relative_y);
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
        self.anchor_menu();
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
        self.anchor_menu();
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
        let scope = crate::host::edit_scope(&self.endpoint, &self.active_channel, seq);
        let choices = crate::host::composer_choices(&self.channel_members);
        self.composers
            .entry(scope)
            .or_default()
            .seed(&seed, &choices);
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
        self.anchor_menu();
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
        self.anchor_menu();
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
        self.anchor_menu();
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
        let scope = crate::host::edit_scope(&self.endpoint, &self.active_channel, seq);
        let choices = crate::host::composer_choices(&self.channel_members);
        self.composers
            .entry(scope)
            .or_default()
            .seed(&seed, &choices);
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
        self.anchor_menu();
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
