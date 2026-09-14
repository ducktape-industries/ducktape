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
/// A compact ghost control: `glyph` (an emoji, or an emoji and one word) is
/// what shows; `label` is what a screen reader and a test press.
fn glyph(key: String, glyph: &str, label: &str, message: Message, disabled: bool) -> wire::Node {
    let mut button = subtle(key, glyph, message, disabled);
    if let wire::Node::Button {
        label: accessible,
        padding,
        ..
    } = &mut button
    {
        *accessible = Some(label.into());
        *padding = Some(wire::Edges {
            top: 2.,
            right: 6.,
            bottom: 2.,
            left: 6.,
        });
    }
    button
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
        // A 10px grab strip with the hairline down its middle.
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
                *width = Some(wire::Length::Fixed(10.));
                *height = Some(wire::Length::Fill);
                *align_x = Some(wire::AlignX::Center);
            }
            strip
        }),
    }
}
/// A section label over a list: a 28px row, the name quiet, and at most one
/// ghost control beside it.
fn section_row(key: String, name: &str, control: Option<wire::Node>) -> wire::Node {
    let mut children = vec![native::sized(
        native::nowrap(native::label(format!("{key}/label"), name)),
        Some(wire::Length::Fill),
        None,
    )];
    children.extend(control);
    native::padded(
        native::sized(
            native::spaced(native::centered_row(key, children), 4.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(28.)),
        ),
        wire::Edges {
            top: 0.,
            right: 4.,
            bottom: 0.,
            left: 8.,
        },
    )
}
/// A pane's title row: a heading, what stands beside it, and its close.
fn pane_header(key: String, children: impl IntoIterator<Item = wire::Node>) -> wire::Node {
    native::padded(
        native::sized(
            native::centered_row(key, children),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(40.)),
        ),
        wire::Edges {
            top: 0.,
            right: 8.,
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
        let (mark, name) = if self.channel_create_open {
            ("✕", "Close")
        } else {
            ("+", "New channel")
        };
        let mut rooms = vec![section_row(
            format!("{key}/channels-header"),
            "Channels",
            Some(glyph(
                format!("{key}/new-channel"),
                mark,
                name,
                Message::ToggleChannelCreate,
                self.loading || self.busy,
            )),
        )];
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
            rooms.push(native::gap(8.));
            rooms.push(section_row(
                format!("{key}/dm-heading-row"),
                "Direct messages",
                None,
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
                native::spaced(native::column(format!("{key}/sidebar-top"), top), 6.),
                wire::Edges::all(8.),
            ),
            native::scroll(
                format!("{key}/rooms"),
                native::padded(
                    native::spaced(native::column(format!("{key}/room-list"), rooms), 2.),
                    wire::Edges {
                        top: 4.,
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
            header.push(native::nowrap(native::colored(
                native::heading(format!("{key}/room-hash"), "#"),
                native::palette().muted,
            )));
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
        header.push(glyph(
            format!("{key}/details"),
            "Details",
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
            // an edit sits under the stream, in place of the composer's
            // attention; every other menu floats at the pointer
            let editing_here =
                self.selected_message_seq > 0 && self.message_action == MessageAction::Editing;
            if editing_here {
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
                wire::SurfaceValue::Str(self.composer_hint()),
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
    /// The room a thread belongs to, as the thread pane's caption.
    fn thread_room_label(&self) -> String {
        let direct = !self.active_dm.name.is_empty();
        match direct {
            true => self.active_dm.name.clone(),
            false => format!("#{}", self.active_channel_name),
        }
    }
    /// The composer's placeholder names the room it posts to.
    fn composer_hint(&self) -> String {
        let direct = !self.active_dm.name.is_empty();
        match direct {
            true => format!("Message {}", self.active_dm.name),
            false => format!("Message #{}", self.active_channel_name),
        }
    }
    fn search_results(&self, key: String) -> wire::Node {
        let children = match self.search_phase {
            SearchPhase::Searching => vec![self.loading_messages(format!("{key}/loading"))],
            SearchPhase::Done if self.search_hits.is_empty() => {
                vec![native::empty_state(
                    format!("{key}/empty"),
                    "No messages match",
                    "Try other words, or clear the search to see the room again.",
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
    /// The head of a room's history: what the room is, and that this is where
    /// it begins. Only the timeline carries it, and only once the whole
    /// history is on screen.
    fn stream_intro(&self, key: String) -> Option<wire::Node> {
        let whole_history = !self.has_older_history && !self.messages.is_empty();
        if !whole_history {
            return None;
        }
        let direct = !self.active_dm.name.is_empty();
        let (name, detail) = if direct {
            (
                self.active_dm.name.clone(),
                format!(
                    "This is the very beginning of your conversation with {}.",
                    self.active_dm.name
                ),
            )
        } else {
            (
                format!("#{}", self.active_channel_name),
                format!(
                    "This is the very beginning of #{}. Say hello, or pin what the room is for.",
                    self.active_channel_name
                ),
            )
        };
        Some(native::padded(
            native::spaced(
                native::column(
                    key.clone(),
                    [
                        native::text_size(native::title(format!("{key}/name"), name), 20.),
                        native::wrapping(native::secondary(format!("{key}/detail"), detail)),
                        native::gap(4.),
                        native::divider(format!("{key}/rule")),
                    ],
                ),
                6.,
            ),
            wire::Edges {
                top: 24.,
                right: 16.,
                bottom: 8.,
                left: 16.,
            },
        ))
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
            // a live run answers the message, so its hint rides under the card
            let mut run_hints = Vec::new();
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
                    run_hints.push(native::padded(
                        content,
                        wire::Edges {
                            top: 2.,
                            right: 16.,
                            bottom: 2.,
                            left: super::kit::RAIL,
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
                // The bar floats over the message, so each control is a
                // glyph; the words ride as the accessible label.
                let mut controls = Vec::new();
                if !thread && message.reply_count == 0 {
                    controls.push(glyph(
                        format!("{scope}/thread"),
                        "💬",
                        "Open thread",
                        Message::OpenThreadFor(message.seq),
                        false,
                    ));
                }
                controls.push(glyph(
                    format!("{scope}/thumbs-up"),
                    "👍",
                    "React with 👍",
                    Message::AddReactionAt(message.seq, "👍".into()),
                    self.active_channel_archived,
                ));
                controls.extend([
                    glyph(
                        format!("{scope}/react"),
                        "😀",
                        "Manage reactions",
                        reaction,
                        self.active_channel_archived,
                    ),
                    glyph(
                        format!("{scope}/more"),
                        "⋯",
                        "More message actions",
                        more.clone(),
                        false,
                    ),
                ]);
                // The actions float over the card's top-right corner while
                // the pointer is on it (or the message is chosen).
                // the pointer washes the row it is on, as the bar appears
                // (the wash is a layer over the text, so it stays faint)
                let mut wash = native::palette().surface_raised;
                wash[3] = 0.25;
                let hover = wire::Node::Hover {
                    key: format!("{scope}/hover"),
                    width: Some(wire::Length::Fill),
                    height: None,
                    padding: None,
                    background: None,
                    border: None,
                    tint: (!target).then_some(wire::Rgba(wash)),
                    radius: 0.,
                    open: target,
                    children: vec![
                        card,
                        self.floating_actions(format!("{scope}/actions"), controls),
                    ],
                };
                children.push(hover);
                children.extend(run_hints);
                let content =
                    native::spaced(native::column(format!("{scope}/content"), children), 0.);
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
                children.extend(run_hints);
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
        // The intro rides inside the scroll, above the keyed rows: the host
        // gives a virtual column and the chrome around it one viewport, so the
        // stream keeps the anchoring it has.
        let intro = (surface == CopySurface::Timeline)
            .then(|| self.stream_intro(format!("{key}/intro")))
            .flatten();
        let content = match intro {
            Some(intro) => native::spaced(native::column(format!("{key}/lead"), [intro, list]), 0.),
            None => list,
        };
        let mut scroll = native::scroll(key, content);
        if let wire::Node::Scroll {
            virtual_rows,
            anchor_y,
            on_scroll,
            ..
        } = &mut scroll
        {
            *virtual_rows = true;
            // a room grows upward from its composer; a thread reads down
            // from its root, and new replies are paged in after it
            *anchor_y = if thread {
                wire::ScrollAnchor::Start
            } else {
                wire::ScrollAnchor::End
            };
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
                            p.accent_foreground,
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
                        native::spaced(
                            native::centered_row(
                                format!("{key}/title-row"),
                                [
                                    native::nowrap(native::heading(
                                        format!("{key}/title"),
                                        "Thread",
                                    )),
                                    native::nowrap(native::caption(
                                        format!("{key}/room"),
                                        self.thread_room_label(),
                                    )),
                                ],
                            ),
                            8.,
                        ),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    glyph(
                        format!("{key}/close"),
                        "✕",
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
        let editing_here =
            self.thread_selected_seq > 0 && self.thread_message_action == MessageAction::Editing;
        if editing_here {
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
                wire::SurfaceValue::Str("Reply in thread".into()),
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
        // the room by its name, its badges beside it, and the link action
        // on its own left-aligned row (a lone button would centre itself)
        let mut title = vec![native::nowrap(native::heading(
            format!("{key}/name"),
            format!("#{}", self.active_channel_name),
        ))];
        if self.active_channel_archived {
            title.push(self.archived_badge(format!("{key}/archived")));
        }
        if self.active_channel_members_only {
            title.push(self.private_badge(format!("{key}/private")));
        }
        let about = vec![
            native::spaced(native::centered_row(format!("{key}/title-row"), title), 8.),
            native::row(
                format!("{key}/link-row"),
                [glyph(
                    format!("{key}/link"),
                    "🔗 Copy link",
                    "Copy channel link",
                    Message::CopyToClipboard(
                        crate::host::duck_channel_link(
                            self.active_channel.clone(),
                            self.network_chain_id.clone(),
                        ),
                        "Channel link copied".into(),
                    ),
                    false,
                )],
            ),
        ];
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
                    glyph(
                        format!("{key}/close"),
                        "✕",
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
    /// Which message menu is open, and on which surface.
    fn open_menu(&self) -> OpenMenu {
        let timeline = self.selected_message_seq > 0 && self.message_action != MessageAction::Toolbar;
        let thread = self.thread_selected_seq > 0 && self.thread_message_action != MessageAction::Toolbar;
        match (timeline, thread) {
            (true, _) => OpenMenu::Timeline(self.message_action),
            (false, true) => OpenMenu::Thread(self.thread_message_action),
            (false, false) => OpenMenu::None,
        }
    }
    /// What closes the open menu.
    pub(super) fn close_menu(&self) -> Message {
        match self.open_menu() {
            OpenMenu::Thread(_) => Message::ClearThreadMessageSelection,
            OpenMenu::Timeline(_) | OpenMenu::None => Message::ClearMessageSelection,
        }
    }
    /// The menu that floats at the pointer: the "…" list, the reaction
    /// picker, or the delete confirmation. An edit is not one — it sits
    /// under its stream.
    pub(super) fn floating_menu(&self, key: &str) -> Option<wire::Node> {
        let (mode, thread) = match self.open_menu() {
            OpenMenu::None => return None,
            OpenMenu::Timeline(mode) => (mode, false),
            OpenMenu::Thread(mode) => (mode, true),
        };
        let size = match mode {
            MessageAction::More => menu_size(if thread { 4 } else { 5 }),
            MessageAction::Reactions => picker_size(crate::host::reaction_palette().len()),
            MessageAction::Delete => (280., 96.),
            MessageAction::Toolbar | MessageAction::Editing => return None,
        };
        let (x, y) = crate::host::menu_origin(
            (self.menu_x, self.menu_y),
            size,
            (self.chat_viewport_width, self.chat_viewport_height),
        );
        let pane = if thread {
            format!("{key}/thread-pane")
        } else {
            key.to_owned()
        };
        // a dropped shadow lifts the card off the stream; deeper on ink
        let shade = if native::is_dark() { 0.5 } else { 0.16 };
        Some(wire::Node::Float {
            key: format!("{key}/floating-menu"),
            x: wire::FloatExpression {
                ops: vec![wire::FloatOp::Number(x)],
            },
            y: wire::FloatExpression {
                ops: vec![wire::FloatOp::Number(y)],
            },
            scale: 1.,
            shadow: wire::Shadow {
                color: Some(wire::Rgba([0., 0., 0., shade])),
                x: Some(0.),
                y: Some(4.),
                blur: Some(16.),
            },
            radius: Some([native::radius::CARD as f32; 4]),
            content: Box::new(native::sized(
                self.message_menu(&pane, thread),
                Some(wire::Length::Fixed(size.0 as f32)),
                None,
            )),
        })
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
                // A dropdown: one item a row, the words left-aligned.
                let mut items = Vec::new();
                if !thread {
                    items.push(menu_item(
                        format!("{key}/{prefix}reply"),
                        "↩",
                        "Reply in thread",
                        Message::OpenThreadFor(seq),
                        false,
                    ));
                }
                items.extend([
                    menu_item(
                        format!("{key}/{prefix}add-reaction"),
                        "😀",
                        "Add reaction",
                        reaction,
                        self.active_channel_archived,
                    ),
                    menu_item(
                        format!("{key}/{prefix}copy-link"),
                        "🔗",
                        "Copy link",
                        Message::CopyMessageLink(crate::host::duck_channel_message_link(
                            self.active_channel.clone(),
                            seq,
                            self.network_chain_id.clone(),
                        )),
                        false,
                    ),
                    menu_item(
                        format!("{key}/{prefix}edit"),
                        "✎",
                        "Edit message",
                        edit,
                        self.active_channel_archived,
                    ),
                    menu_item(
                        format!("{key}/{prefix}delete"),
                        "🗑",
                        "Delete message",
                        delete,
                        self.active_channel_archived,
                    ),
                ]);
                children.push(native::spaced(
                    native::column(format!("{key}/{prefix}menu-actions"), items),
                    MENU_ITEM_GAP,
                ));
            }
            MessageAction::Reactions => {
                let mut choices = Vec::new();
                for emoji in crate::host::reaction_palette() {
                    choices.push(emoji_cell(
                        format!("{key}/{prefix}reaction/{emoji}"),
                        &emoji,
                        Message::AddReactionAt(seq, emoji.clone()),
                        self.active_channel_archived,
                    ));
                }
                // the host cuts cells from the grid's measured width, so the
                // grid states its width: eight cells and seven gaps
                let columns = PICKER_COLUMNS as f32;
                children.push(wire::Node::Grid {
                    key: format!("{key}/{prefix}reaction-grid"),
                    columns: Some(PICKER_COLUMNS),
                    fluid: None,
                    spacing: Some(PICKER_GAP),
                    padding: None,
                    width: Some(wire::Length::Fixed(
                        columns * PICKER_CELL + (columns - 1.) * PICKER_GAP,
                    )),
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
                children.push(native::aligned(
                    native::column(
                        format!("{key}/{prefix}close-row"),
                        [subtle(
                            format!("{key}/{prefix}close"),
                            "Cancel message edit",
                            close,
                            self.busy,
                        )],
                    ),
                    wire::AlignX::Right,
                ));
            }
            MessageAction::Delete => {
                children.push(native::strong(
                    format!("{key}/{prefix}confirm"),
                    "Delete this message?",
                ));
                children.push(native::wrapping(native::secondary(
                    format!("{key}/{prefix}confirm-detail"),
                    "It leaves the room for everyone.",
                )));
                children.push(native::aligned(
                    native::spaced(
                        native::row(
                            format!("{key}/{prefix}confirm-row"),
                            [
                                native::spacer(),
                                subtle(format!("{key}/{prefix}close"), "Cancel", close, false),
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
                        ),
                        6.,
                    ),
                    wire::AlignX::Right,
                ));
            }
        }
        // The frame is what the host focuses when the menu opens; only the
        // edit sits in the stream's flow and wears the stream's inset.
        let frame_key = format!("{key}/{prefix}{focus}");
        let menu = native::spaced(native::column(format!("{key}/{prefix}menu"), children), 8.);
        let mut frame = native::card(frame_key, menu);
        if let wire::Node::Container { padding, .. } = &mut frame {
            *padding = Some(wire::Edges::all(match mode {
                MessageAction::Toolbar | MessageAction::More => MENU_INSET,
                MessageAction::Reactions => PICKER_INSET,
                MessageAction::Editing | MessageAction::Delete => 12.,
            }));
        }
        match mode {
            MessageAction::Editing => native::padded(
                frame,
                wire::Edges {
                    top: 4.,
                    right: 16.,
                    bottom: 4.,
                    left: 16.,
                },
            ),
            MessageAction::Toolbar
            | MessageAction::More
            | MessageAction::Reactions
            | MessageAction::Delete => frame,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMenu {
    None,
    Timeline(MessageAction),
    Thread(MessageAction),
}
const MENU_ITEM_HEIGHT: f32 = 28.;
const MENU_ITEM_GAP: f32 = 2.;
const MENU_INSET: f32 = 6.;
const PICKER_COLUMNS: u32 = 8;
const PICKER_CELL: f32 = 32.;
const PICKER_GAP: f32 = 2.;
const PICKER_INSET: f32 = 8.;
/// The "…" dropdown's box for `items` rows.
fn menu_size(items: usize) -> (f64, f64) {
    let rows = items as f32;
    let height = MENU_INSET * 2. + rows * MENU_ITEM_HEIGHT + (rows - 1.).max(0.) * MENU_ITEM_GAP;
    (220., f64::from(height))
}
/// The reaction picker's box for `count` emoji, eight to a row.
fn picker_size(count: usize) -> (f64, f64) {
    let columns = PICKER_COLUMNS as f32;
    let rows = (count as f32 / columns).ceil();
    let width = PICKER_INSET * 2. + columns * PICKER_CELL + (columns - 1.) * PICKER_GAP;
    let height = PICKER_INSET * 2. + rows * PICKER_CELL + (rows - 1.).max(0.) * PICKER_GAP;
    (f64::from(width), f64::from(height))
}
/// One row of a dropdown: a glyph, then the words, left-aligned across the
/// menu's width.
fn menu_item(key: String, glyph: &str, label: &str, message: Message, disabled: bool) -> wire::Node {
    let content = native::spaced(
        native::centered_row(
            format!("{key}/row"),
            [
                native::sized(
                    native::nowrap(native::text(format!("{key}/glyph"), glyph)),
                    Some(wire::Length::Fixed(20.)),
                    None,
                ),
                native::nowrap(native::text(format!("{key}/label"), label)),
            ],
        ),
        8.,
    );
    let mut button = native::button_child(
        key,
        content,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Subtle,
    );
    if let wire::Node::Button {
        label: accessible,
        width,
        height,
        padding,
        ..
    } = &mut button
    {
        *accessible = Some(label.into());
        *width = Some(wire::Length::Fill);
        *height = Some(wire::Length::Fixed(MENU_ITEM_HEIGHT));
        *padding = Some(wire::Edges {
            top: 0.,
            right: 8.,
            bottom: 0.,
            left: 8.,
        });
    }
    button
}
/// One cell of the reaction picker: a fixed square with the emoji centred
/// in it, tall enough that the glyph's full height paints inside.
fn emoji_cell(key: String, emoji: &str, message: Message, disabled: bool) -> wire::Node {
    let mut button = subtle(key, emoji, message, disabled);
    if let wire::Node::Button {
        label,
        description,
        width,
        height,
        padding,
        ..
    } = &mut button
    {
        *label = Some("Add reaction".into());
        *description = Some(emoji.into());
        *width = Some(wire::Length::Fixed(PICKER_CELL));
        *height = Some(wire::Length::Fixed(PICKER_CELL));
        *padding = Some(wire::Edges::all(0.));
    }
    button
}
