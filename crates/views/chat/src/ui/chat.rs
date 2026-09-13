use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

fn action(key: String, label: &str, message: Message, disabled: bool) -> wire::Node {
    native::button(
        key,
        label,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Secondary,
    )
}
/// A quiet action: a ghost button for what sits beside content.
fn subtle(key: String, label: &str, message: Message, disabled: bool) -> wire::Node {
    native::button(
        key,
        label,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Subtle,
    )
}
fn primary(key: String, label: &str, message: Message, disabled: bool) -> wire::Node {
    native::button(
        key,
        label,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Primary,
    )
}
fn field(
    key: String,
    label: &str,
    value: &str,
    route: fn(String) -> Message,
    submit: Option<Message>,
    disabled: bool,
) -> wire::Node {
    let mut node = native::input(
        key,
        label,
        value,
        slots::handler::<String, Message>(Box::new(move |value| Some(route(value)))),
        submit.map(slots::message),
    );
    if let wire::Node::Input { options, .. } = &mut node {
        options.label = label.into();
        options.disabled = disabled;
    }
    node
}
fn divider(key: String, route: fn(f64, f64) -> Message) -> wire::Node {
    wire::Node::ResizeHandle {
        key: key.clone(),
        on_press: None,
        on_release: None,
        on_drag: Some(slots::handler::<(f64, f64), Message>(Box::new(
            move |(x, y)| Some(route(x, y)),
        ))),
        cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
        // An 8px grab strip with the hairline down its middle.
        content: Box::new({
            let mut strip = native::container(
                format!("{key}/strip"),
                native::vertical_divider(format!("{key}/rule")),
            );
            if let wire::Node::Container {
                width,
                height,
                align_x,
                ..
            } = &mut strip
            {
                *width = Some(wire::Length::Fixed(8.));
                *height = Some(wire::Length::Fill);
                *align_x = Some(wire::AlignX::Center);
            }
            strip
        }),
    }
}
/// A pane's title row: a heading, what stands beside it, and its close.
fn pane_header(key: String, children: impl IntoIterator<Item = wire::Node>) -> wire::Node {
    native::padded(
        native::sized(
            native::centered_row(key, children),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(48.)),
        ),
        wire::Edges {
            top: 0.,
            right: 12.,
            bottom: 0.,
            left: 16.,
        },
    )
}
impl ChatView {
    pub(super) fn chat_screen(&self, key: String) -> wire::Node {
        if !self.connected {
            return self.disconnected(format!("{key}/disconnected"));
        }
        let mut panes = vec![
            self.sidebar(&key),
            divider(format!("{key}/sidebar-resize"), Message::SidebarResized),
            self.room(&key),
        ];
        if self.channel_settings_open && !self.active_channel.is_empty() {
            panes.push(divider(
                format!("{key}/details-resize"),
                Message::DetailsResized,
            ));
            panes.push(self.channel_details(format!("{key}/details-pane")));
        }
        if self.active_thread_seq > 0 && !self.active_channel.is_empty() {
            panes.push(divider(
                format!("{key}/thread-resize"),
                Message::ThreadResized,
            ));
            panes.push(self.thread(format!("{key}/thread-pane")));
        }
        native::sized(
            native::spaced(native::row(key, panes), 0.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }
    fn sidebar(&self, key: &str) -> wire::Node {
        let mut search = field(
            format!("{key}/channel-sidebar/chat-search"),
            "Search messages",
            &self.search_draft,
            Message::SearchDraftChanged,
            Some(Message::SearchChatSubmit),
            false,
        );
        if let wire::Node::Input { placeholder, .. } = &mut search {
            *placeholder = "Search messages…".into();
        }
        let mut top = vec![search];
        let search_active =
            self.search_phase != SearchPhase::Idle || !self.search_draft.trim().is_empty();
        if search_active {
            top.push(subtle(
                format!("{key}/clear-search"),
                "Clear message search",
                Message::ClearChatSearch,
                false,
            ));
        }
        top.push(native::centered_row(
            format!("{key}/channels-header"),
            [
                native::sized(
                    native::heading(format!("{key}/channels-label"), "Channels"),
                    Some(wire::Length::Fill),
                    None,
                ),
                subtle(
                    format!("{key}/new-channel"),
                    if self.channel_create_open {
                        "Close"
                    } else {
                        "New channel"
                    },
                    Message::ToggleChannelCreate,
                    self.loading || self.busy,
                ),
            ],
        ));
        let mut rooms = Vec::new();
        for room in &self.rooms {
            rooms.push(self.channel_button(
                format!("{key}/channel/{}", room.channel.id),
                Message::ChooseChannel,
                room.channel.clone(),
                room.channel.id == self.active_channel,
                room.unread,
            ));
        }
        if !self.dm_rows.is_empty() {
            rooms.push(native::padded(
                native::column(
                    format!("{key}/dm-heading-row"),
                    [native::heading(format!("{key}/dm-heading"), "Direct messages")],
                ),
                wire::Edges {
                    top: 16.,
                    right: 0.,
                    bottom: 4.,
                    left: 0.,
                },
            ));
        }
        for row in &self.dm_rows {
            rooms.push(self.direct_message(
                format!("{key}/dm/{}", row.peer.key),
                Message::ChooseDm,
                row.peer.clone(),
                row.peer.key == self.active_dm_peer,
                row.unread,
            ));
        }
        let children = vec![
            native::padded(
                native::spaced(native::column(format!("{key}/sidebar-top"), top), 10.),
                wire::Edges {
                    top: 12.,
                    right: 12.,
                    bottom: 4.,
                    left: 12.,
                },
            ),
            native::scroll(
                format!("{key}/rooms"),
                native::padded(
                    native::spaced(native::column(format!("{key}/room-list"), rooms), 1.),
                    wire::Edges {
                        top: 0.,
                        right: 8.,
                        bottom: 12.,
                        left: 8.,
                    },
                ),
            ),
        ];
        native::pane(
            format!("{key}/channel-sidebar"),
            native::spaced(
                native::sized(
                    native::column(format!("{key}/sidebar-content"), children),
                    Some(wire::Length::Fill),
                    Some(wire::Length::Fill),
                ),
                0.,
            ),
            wire::Length::Fixed(self.sidebar_width as f32),
        )
    }
    fn room(&self, key: &str) -> wire::Node {
        let mut header = Vec::new();
        if self.active_dm.name.is_empty() {
            header.push(native::nowrap(native::heading(
                format!("{key}/room-name"),
                &self.active_channel_name,
            )));
        } else {
            header.push(self.direct_message_header(format!("{key}/dm-header")));
        }
        if self.active_channel_archived {
            header.push(self.archived_badge(format!("{key}/archived")));
        }
        if self.active_channel_members_only {
            header.push(self.private_badge(format!("{key}/private")));
        }
        header.push(native::spacer());
        if self.huddle_joined {
            header.push(self.huddle_controls(
                format!("{key}/huddle"),
                || Message::LeaveHuddleHere,
                || Message::ShowHuddle,
            ));
        } else if !self.active_channel.is_empty() {
            header.push(self.start_huddle(format!("{key}/huddle"), || Message::JoinHuddleSubmit));
        }
        header.push(subtle(
            format!("{key}/details"),
            "Channel details",
            Message::ToggleChannelSettings,
            self.active_channel.is_empty(),
        ));
        let mut children = vec![
            pane_header(format!("{key}/header"), header),
            native::divider(format!("{key}/header-rule")),
        ];
        if !self.host_error.is_empty() {
            children.push(native::padded(
                native::notice(
                    format!("{key}/error"),
                    native::wrapping(native::text(format!("{key}/error-text"), &self.host_error)),
                    Tone::Danger,
                ),
                wire::Edges::all(12.),
            ));
        }
        let query_matches =
            !self.search_query.is_empty() && self.search_draft.trim() == self.search_query;
        let search_stands = query_matches
            && (self.search_phase == SearchPhase::Searching
                || crate::host::search_answer_stands(
                    &self.search_query,
                    &self.search_draft,
                    false,
                ));
        if search_stands {
            children.push(self.search_results(format!("{key}/search-results")));
        } else {
            if self.loading && self.messages.is_empty() {
                children.push(self.loading_messages(format!("{key}/loading")));
            }
            if !self.loading && self.messages.is_empty() {
                children.push(self.empty_messages(format!("{key}/empty")));
            }
            if self.has_older_history {
                children.push(native::padded(
                    native::aligned(
                        native::column(
                            format!("{key}/older-row"),
                            [subtle(
                                format!("{key}/older"),
                                if self.history_loading {
                                    "Loading older messages…"
                                } else {
                                    "Load older messages"
                                },
                                Message::LoadMoreHistory,
                                self.loading || self.history_loading || self.busy,
                            )],
                        ),
                        wire::AlignX::Center,
                    ),
                    wire::Edges::all(8.),
                ));
            }
            children.push(self.message_list(
                format!("{key}/message-stream"),
                &self.messages,
                CopySurface::Timeline,
            ));
            if self.copy_surface == CopySurface::Timeline {
                children.push(self.selection_bar(format!("{key}/copy-range"), &self.messages));
            }
            if !self.messages.is_empty() && (self.history_view || !self.at_live_tail) {
                children.push(native::padded(
                    native::aligned(
                        native::column(
                            format!("{key}/latest-row"),
                            [action(
                                format!("{key}/latest"),
                                "Jump to latest",
                                Message::ChooseChannel(self.active_channel.clone()),
                                false,
                            )],
                        ),
                        wire::AlignX::Center,
                    ),
                    wire::Edges {
                        top: 4.,
                        right: 16.,
                        bottom: 4.,
                        left: 16.,
                    },
                ));
            }
            if self.selected_message_seq > 0 {
                children.push(self.message_menu(key, false));
            }
        }
        if !self.post_refusal.is_empty() {
            children.push(native::padded(
                self.composer_gate(format!("{key}/refusal")),
                wire::Edges {
                    top: 0.,
                    right: 16.,
                    bottom: 8.,
                    left: 16.,
                },
            ));
        }
        children.push(wire::Node::Surface {
            key: format!("{key}/composer"),
            name: "chat_composer".into(),
            args: vec![
                wire::SurfaceValue::Str(crate::host::composer_scope(
                    &self.endpoint,
                    &self.active_channel,
                )),
                wire::SurfaceValue::Str("message".into()),
                wire::SurfaceValue::Bool(false),
                wire::SurfaceValue::Str("Message the channel…".into()),
                wire::SurfaceValue::Bool(
                    self.loading
                        || !self.connected
                        || self.active_channel.is_empty()
                        || !self.post_refusal.is_empty(),
                ),
                wire::SurfaceValue::Bool(self.busy),
                wire::SurfaceValue::Str("An earlier message wasn’t sent".into()),
            ],
            on_event: None,
        });
        native::sized(
            native::spaced(native::column(format!("{key}/room"), children), 0.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }
    fn search_results(&self, key: String) -> wire::Node {
        let children = match self.search_phase {
            SearchPhase::Searching => vec![self.loading_messages(format!("{key}/loading"))],
            SearchPhase::Done if self.search_hits.is_empty() => {
                vec![native::padded(
                    native::column(
                        format!("{key}/empty-row"),
                        [native::secondary(format!("{key}/empty"), "No messages match")],
                    ),
                    wire::Edges::all(16.),
                )]
            }
            SearchPhase::Done => self
                .search_hits
                .iter()
                .map(|hit| {
                    self.search_result(
                        format!("{key}/{}/{}", hit.channel_id, hit.seq),
                        Message::OpenChatSearchHit,
                        hit.clone(),
                    )
                })
                .collect(),
            SearchPhase::Idle => Vec::new(),
        };
        native::scroll(
            key.clone(),
            native::padded(
                native::spaced(native::column(format!("{key}/rows"), children), 2.),
                wire::Edges::all(8.),
            ),
        )
    }
    fn message_list(
        &self,
        key: String,
        messages: &[crate::host::ChatMessage],
        surface: CopySurface,
    ) -> wire::Node {
        let thread = surface == CopySurface::Thread;
        let selected = if thread {
            self.thread_selected_seq
        } else {
            self.selected_message_seq
        };
        let mut keys = Vec::new();
        let mut rows = Vec::new();
        for message in messages {
            let scope = format!("{key}/message/{}", message.view_key);
            let ranged = crate::host::seq_in_copy_range(
                message.seq,
                self.copy_anchor_seq,
                self.copy_head_seq,
                self.copy_surface,
                surface,
            );
            let target =
                selected == message.seq || (thread && self.thread_target_seq == message.seq);
            let plate = crate::host::message_plate(message.deleted, target, ranged);
            let mut children = Vec::new();
            if !thread && self.unread_boundary > 0 && message.seq == self.unread_marker_seq {
                children.push(self.unread_marker(format!("{scope}/unread")));
            }
            let card = self.message_card(message, surface, plate);
            for live in &self.live_agents {
                if crate::host::run_in_thread(live, message.seq) {
                    let run_key = format!("{scope}/run/{}", live.agent);
                    let content = if thread {
                        self.live_run_card(
                            run_key,
                            Message::CancelRun,
                            Message::OpenRun,
                            live.clone(),
                        )
                    } else {
                        subtle(
                            run_key,
                            &crate::host::live_thread_label(&live.agent),
                            Message::OpenThreadFor(message.seq),
                            false,
                        )
                    };
                    children.push(native::padded(
                        content,
                        wire::Edges {
                            top: 2.,
                            right: 16.,
                            bottom: 2.,
                            left: 54.,
                        },
                    ));
                }
            }
            let actions = if thread {
                [
                    Message::OpenThreadMessageReactions(
                        message.seq,
                        message.body.clone(),
                        message.rev,
                    ),
                    Message::OpenThreadMessageActions(
                        message.seq,
                        message.body.clone(),
                        message.rev,
                    ),
                ]
            } else {
                [
                    Message::OpenMessageReactions(message.seq, message.body.clone(), message.rev),
                    Message::OpenMessageActions(message.seq, message.body.clone(), message.rev),
                ]
            };
            let [reaction, more] = actions;
            if !message.pending && !message.deleted {
                let mut controls = Vec::new();
                if !thread && message.reply_count == 0 {
                    controls.push(subtle(
                        format!("{scope}/thread"),
                        "Open thread",
                        Message::OpenThreadFor(message.seq),
                        false,
                    ));
                }
                controls.push(subtle(
                    format!("{scope}/thumbs-up"),
                    "React with 👍",
                    Message::AddReactionAt(message.seq, "👍".into()),
                    self.active_channel_archived,
                ));
                controls.extend([
                    subtle(
                        format!("{scope}/react"),
                        "Manage reactions",
                        reaction,
                        self.active_channel_archived,
                    ),
                    subtle(
                        format!("{scope}/more"),
                        "More message actions",
                        more.clone(),
                        false,
                    ),
                ]);
                // The actions float over the card's top-right corner while
                // the pointer is on it (or the message is chosen).
                let hover = wire::Node::Hover {
                    key: format!("{scope}/hover"),
                    width: Some(wire::Length::Fill),
                    height: None,
                    padding: None,
                    background: None,
                    border: None,
                    tint: None,
                    radius: 0.,
                    open: target,
                    children: vec![card, self.floating_actions(format!("{scope}/actions"), controls)],
                };
                children.push(hover);
                let content = native::spaced(native::column(format!("{scope}/content"), children), 0.);
                rows.push(wire::Node::MouseArea {
                    key: scope,
                    on_press: None,
                    on_release: None,
                    on_double_click: None,
                    on_right_press: Some(slots::message(more)),
                    on_right_release: None,
                    on_middle_press: None,
                    on_middle_release: None,
                    on_enter: None,
                    on_exit: None,
                    on_move: None,
                    on_press_at: None,
                    on_scroll: None,
                    content: Box::new(content),
                });
            } else {
                children.push(card);
                rows.push(native::spaced(native::column(scope, children), 0.));
            }
            keys.push(wire::ListKey::from(message.view_key));
        }
        let list = wire::Node::KeyedColumn {
            key: format!("{key}/rows"),
            keys: Some(keys),
            children: rows,
            background: None,
            border: None,
            spacing: None,
            padding: Some(wire::Edges {
                top: 8.,
                right: 0.,
                bottom: 8.,
                left: 0.,
            }),
            width: Some(wire::Length::Fill),
            height: None,
            max_width: None,
            align: None,
            virtual_row: Some(44.0f32),
        };
        let mut scroll = native::scroll(key, list);
        if let wire::Node::Scroll {
            virtual_rows,
            anchor_y,
            on_scroll,
            ..
        } = &mut scroll
        {
            *virtual_rows = true;
            *anchor_y = wire::ScrollAnchor::End;
            if !thread {
                *on_scroll = Some(slots::handler::<(f32, f32, f32, f32), Message>(Box::new(
                    |(x, y, rx, ry)| {
                        Some(Message::ChatScrolled(
                            x.into(),
                            y.into(),
                            rx.into(),
                            ry.into(),
                        ))
                    },
                )));
            }
        }
        scroll
    }
    /// The bar of quiet actions that floats over a message's top-right.
    fn floating_actions(&self, key: String, controls: Vec<wire::Node>) -> wire::Node {
        let p = native::palette();
        let mut bar = native::spaced(native::row(format!("{key}/bar"), controls), 0.);
        if let wire::Node::Linear {
            background,
            border,
            padding,
            width,
            ..
        } = &mut bar
        {
            *background = Some(native::rgba(p.background));
            *border = Some(wire::Border {
                color: Some(native::rgba(p.border)),
                width: Some(1.),
                radius: Some([native::radius::CONTROL as f32; 4]),
            });
            *padding = Some(wire::Edges::all(2.));
            *width = Some(wire::Length::Shrink);
        }
        let mut anchor = native::container(key, bar);
        if let wire::Node::Container {
            align_x,
            align_y,
            padding,
            height,
            ..
        } = &mut anchor
        {
            *align_x = Some(wire::AlignX::Right);
            *align_y = Some(wire::AlignY::Top);
            *height = Some(wire::Length::Fill);
            *padding = Some(wire::Edges {
                top: 0.,
                right: 12.,
                bottom: 0.,
                left: 0.,
            });
        }
        anchor
    }
    /// The line that marks where unread messages begin.
    fn unread_marker(&self, key: String) -> wire::Node {
        let p = native::palette();
        let mut rule = native::divider(format!("{key}/rule"));
        if let wire::Node::Rule { color, .. } = &mut rule {
            *color = Some(native::rgba(p.accent));
        }
        native::padded(
            native::spaced(
                native::centered_row(
                    key.clone(),
                    [
                        native::container(format!("{key}/line"), rule),
                        native::nowrap(native::colored(
                            native::caption(format!("{key}/label"), "New messages"),
                            p.warning,
                        )),
                    ],
                ),
                8.,
            ),
            wire::Edges {
                top: 6.,
                right: 16.,
                bottom: 6.,
                left: 16.,
            },
        )
    }
    fn selection_bar(&self, key: String, messages: &[crate::host::ChatMessage]) -> wire::Node {
        native::padded(
            native::notice(
                key.clone(),
                native::centered_row(
                    format!("{key}/row"),
                    [
                        native::sized(
                            native::strong(
                                format!("{key}/count"),
                                crate::host::copy_range_label(crate::host::copy_range_count(
                                    messages,
                                    self.copy_anchor_seq,
                                    self.copy_head_seq,
                                )),
                            ),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        subtle(
                            format!("{key}/clear"),
                            "Clear",
                            Message::ClearCopyRange,
                            false,
                        ),
                        primary(
                            format!("{key}/copy-range"),
                            "Copy",
                            Message::CopySelectedMessages,
                            false,
                        ),
                    ],
                ),
                Tone::Accent,
            ),
            wire::Edges {
                top: 4.,
                right: 16.,
                bottom: 4.,
                left: 16.,
            },
        )
    }
    fn thread(&self, key: String) -> wire::Node {
        let mut children = vec![
            pane_header(
                format!("{key}/header"),
                [
                    native::sized(
                        native::heading(format!("{key}/title"), "Thread"),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    subtle(
                        format!("{key}/close"),
                        "Close thread",
                        Message::CloseThread,
                        false,
                    ),
                ],
            ),
            native::divider(format!("{key}/header-rule")),
        ];
        if self.thread_loading && self.thread_messages.is_empty() {
            children.push(self.loading_messages(format!("{key}/loading")));
        }
        if self.thread_has_more {
            children.push(native::padded(
                subtle(
                    format!("{key}/older"),
                    "Load more replies",
                    Message::LoadMoreThread,
                    self.thread_loading || self.busy,
                ),
                wire::Edges::all(8.),
            ));
        }
        children.push(self.message_list(
            format!("{key}/thread-stream"),
            &self.thread_messages,
            CopySurface::Thread,
        ));
        if self.copy_surface == CopySurface::Thread {
            children.push(self.selection_bar(format!("{key}/copy-range"), &self.thread_messages));
        }
        if self.thread_selected_seq > 0 {
            children.push(self.message_menu(&key, true));
        }
        children.push(wire::Node::Surface {
            key: format!("{key}/reply_composer"),
            name: "chat_composer".into(),
            args: vec![
                wire::SurfaceValue::Str(crate::host::thread_scope(
                    &self.endpoint,
                    &self.active_channel,
                    self.active_thread_seq,
                )),
                wire::SurfaceValue::Str("reply".into()),
                wire::SurfaceValue::Bool(true),
                wire::SurfaceValue::Str("Reply…".into()),
                wire::SurfaceValue::Bool(
                    self.thread_loading || !self.connected || !self.post_refusal.is_empty(),
                ),
                wire::SurfaceValue::Bool(false),
                wire::SurfaceValue::Str("Unsent reply".into()),
            ],
            on_event: None,
        });
        native::pane(
            key.clone(),
            native::sized(
                native::spaced(native::column(format!("{key}/content"), children), 0.),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            ),
            wire::Length::Fixed(self.thread_width as f32),
        )
    }
    fn channel_details(&self, key: String) -> wire::Node {
        let mut about = vec![native::heading(
            format!("{key}/name"),
            &self.active_channel_name,
        )];
        let mut badges = Vec::new();
        if self.active_channel_archived {
            badges.push(self.archived_badge(format!("{key}/archived")));
        }
        if self.active_channel_members_only {
            badges.push(self.private_badge(format!("{key}/private")));
        }
        if !badges.is_empty() {
            about.push(native::row(format!("{key}/badges"), badges));
        }
        about.push(subtle(
            format!("{key}/link"),
            "Copy channel link",
            Message::CopyToClipboard(
                crate::host::duck_channel_link(
                    self.active_channel.clone(),
                    self.network_chain_id.clone(),
                ),
                "Channel link copied".into(),
            ),
            false,
        ));
        let rename = native::column(
            format!("{key}/rename-section"),
            [
                self.name_label(format!("{key}/name-label")),
                native::row(
                    format!("{key}/rename-row"),
                    [
                        field(
                            format!("{key}/name-input"),
                            "Channel name",
                            &self.channel_name_draft,
                            Message::ChannelNameDraftChanged,
                            Some(Message::RenameChannelSubmit),
                            self.busy,
                        ),
                        action(
                            format!("{key}/rename"),
                            "Rename",
                            Message::RenameChannelSubmit,
                            self.busy || self.channel_name_draft.trim().is_empty(),
                        ),
                    ],
                ),
            ],
        );
        let mut members = vec![
            self.members_label(format!("{key}/members-label")),
            native::row(
                format!("{key}/add-row"),
                [
                    field(
                        format!("{key}/member-input"),
                        "Member account or public key",
                        &self.member_key_draft,
                        Message::MemberKeyDraftChanged,
                        Some(Message::AddChannelMemberSubmit),
                        self.busy,
                    ),
                    action(
                        format!("{key}/add-member"),
                        "Add",
                        Message::AddChannelMemberSubmit,
                        self.busy || self.member_key_draft.trim().is_empty(),
                    ),
                ],
            ),
        ];
        if self.channel_members.is_empty() {
            members.push(native::wrapping(native::secondary(format!("{key}/no-members"), "No members added. An open channel needs none — membership only gates posting in a members-only channel.")));
        }
        for member in &self.channel_members {
            members.push(self.member_row(
                format!("{key}/member/{}", member.key),
                Message::RemoveChannelMemberSubmit,
                member.clone(),
            ));
        }
        let (label, message) = if self.active_channel_archived {
            ("Unarchive channel", Message::UnarchiveChannelSubmit)
        } else {
            ("Archive channel", Message::ArchiveChannelSubmit)
        };
        let lifecycle = native::column(
            format!("{key}/lifecycle"),
            [
                native::wrapping(native::secondary(
                    format!("{key}/archive-help"),
                    if self.active_channel_archived {
                        "An archived channel keeps its history and takes no new messages."
                    } else {
                        "Archiving keeps the history and closes the channel to new messages."
                    },
                )),
                action(format!("{key}/archive"), label, message, self.busy),
            ],
        );
        let children = vec![
            pane_header(
                format!("{key}/header"),
                [
                    native::sized(
                        native::heading(format!("{key}/title"), "Channel details"),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    subtle(
                        format!("{key}/close"),
                        "Close channel details",
                        Message::ToggleChannelSettings,
                        false,
                    ),
                ],
            ),
            native::divider(format!("{key}/header-rule")),
            native::scroll(
                format!("{key}/scroll"),
                native::padded(
                    native::spaced(
                        native::column(
                            format!("{key}/content"),
                            [
                                native::column(format!("{key}/about"), about),
                                native::divider(format!("{key}/rule-1")),
                                rename,
                                native::divider(format!("{key}/rule-2")),
                                native::column(format!("{key}/members"), members),
                                native::divider(format!("{key}/rule-3")),
                                lifecycle,
                            ],
                        ),
                        16.,
                    ),
                    wire::Edges::all(16.),
                ),
            ),
        ];
        native::pane(
            key.clone(),
            native::sized(
                native::spaced(native::column(format!("{key}/pane-content"), children), 0.),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            ),
            wire::Length::Fixed(self.details_width as f32),
        )
    }
    fn message_menu(&self, key: &str, thread: bool) -> wire::Node {
        let (seq, rev, body, mode, close) = if thread {
            (
                self.thread_selected_seq,
                self.thread_selected_rev,
                &self.thread_edit_draft,
                self.thread_message_action,
                Message::ClearThreadMessageSelection,
            )
        } else {
            (
                self.selected_message_seq,
                self.selected_message_rev,
                &self.message_edit_draft,
                self.message_action,
                Message::ClearMessageSelection,
            )
        };
        let prefix = if thread { "thread-" } else { "message-" };
        let focus = match mode {
            MessageAction::Reactions => "reaction-focus",
            MessageAction::Delete => "delete-focus",
            _ => "action-focus",
        };
        let mut children = Vec::new();
        let mut tone = Tone::Neutral;
        match mode {
            MessageAction::Toolbar | MessageAction::More => {
                let reaction = if thread {
                    Message::OpenThreadMessageReactions(seq, body.clone(), rev)
                } else {
                    Message::OpenMessageReactions(seq, body.clone(), rev)
                };
                let edit = if thread {
                    Message::BeginThreadMessageEdit(seq, body.clone(), rev)
                } else {
                    Message::BeginMessageEdit(seq, body.clone(), rev)
                };
                let delete = if thread {
                    Message::ArmThreadMessageDelete(seq, body.clone(), rev)
                } else {
                    Message::ArmMessageDelete(seq, body.clone(), rev)
                };
                children.push(native::wrapped_row(
                    format!("{key}/{prefix}menu-actions"),
                    [
                        subtle(
                            format!("{key}/{prefix}add-reaction"),
                            "Add reaction",
                            reaction,
                            self.active_channel_archived,
                        ),
                        subtle(
                            format!("{key}/{prefix}copy-link"),
                            "Copy message link",
                            Message::CopyMessageLink(crate::host::duck_channel_message_link(
                                self.active_channel.clone(),
                                seq,
                                self.network_chain_id.clone(),
                            )),
                            false,
                        ),
                        subtle(
                            format!("{key}/{prefix}edit"),
                            "Edit message",
                            edit,
                            self.active_channel_archived,
                        ),
                        subtle(
                            format!("{key}/{prefix}delete"),
                            "Delete message",
                            delete,
                            self.active_channel_archived,
                        ),
                    ],
                ));
            }
            MessageAction::Reactions => {
                let mut choices = Vec::new();
                for emoji in crate::host::reaction_palette() {
                    let mut button = subtle(
                        format!("{key}/{prefix}reaction/{emoji}"),
                        &emoji,
                        Message::AddReactionAt(seq, emoji.clone()),
                        self.active_channel_archived,
                    );
                    if let wire::Node::Button {
                        label, description, ..
                    } = &mut button
                    {
                        *label = Some("Add reaction".into());
                        *description = Some(emoji);
                    }
                    choices.push(button);
                }
                children.push(wire::Node::Grid {
                    key: format!("{key}/{prefix}reaction-grid"),
                    columns: Some(8),
                    fluid: None,
                    spacing: Some(4.),
                    padding: None,
                    width: Some(wire::Length::Fill),
                    height: None,
                    aspect: None,
                    background: None,
                    border: None,
                    children: choices,
                });
            }
            MessageAction::Editing => {
                children.push(wire::Node::Surface {
                    key: format!("{key}/{prefix}edit-composer"),
                    name: "chat_composer".into(),
                    args: vec![
                        wire::SurfaceValue::Str(crate::host::edit_scope(
                            &self.endpoint,
                            &self.active_channel,
                            seq,
                        )),
                        wire::SurfaceValue::Str(if thread { "thread_edit" } else { "edit" }.into()),
                        wire::SurfaceValue::Bool(true),
                        wire::SurfaceValue::Str("Edit message".into()),
                        wire::SurfaceValue::Bool(self.busy),
                        wire::SurfaceValue::Bool(false),
                        wire::SurfaceValue::Str("Could not save changes".into()),
                    ],
                    on_event: None,
                });
            }
            MessageAction::Delete => {
                tone = Tone::Danger;
                children.push(native::centered_row(
                    format!("{key}/{prefix}confirm-row"),
                    [
                        native::sized(
                            native::strong(
                                format!("{key}/{prefix}confirm"),
                                "Delete this message?",
                            ),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        native::button(
                            format!("{key}/{prefix}confirm-delete"),
                            "Delete",
                            (!self.busy).then(|| {
                                slots::message(if thread {
                                    Message::DeleteThreadMessageSubmit
                                } else {
                                    Message::DeleteMessageSubmit
                                })
                            }),
                            wire::ButtonPreset::Danger,
                        ),
                    ],
                ));
            }
        }
        children.push(native::aligned(
            native::column(
                format!("{key}/{prefix}close-row"),
                [subtle(
                    format!("{key}/{prefix}close"),
                    if mode == MessageAction::Editing {
                        "Cancel message edit"
                    } else {
                        "Cancel"
                    },
                    close,
                    self.busy && mode == MessageAction::Editing,
                )],
            ),
            wire::AlignX::Right,
        ));
        native::padded(
            native::notice(
                format!("{key}/{prefix}{focus}"),
                native::column(format!("{key}/{prefix}menu"), children),
                tone,
            ),
            wire::Edges {
                top: 4.,
                right: 16.,
                bottom: 4.,
                left: 16.,
            },
        )
    }
}
