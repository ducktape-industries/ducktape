use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

/// The avatar plate beside an author's first message, and the gap to the
/// text. A continuation row keeps the same rail so bodies line up.
pub(super) const AVATAR: f32 = 28.;
pub(super) const RAIL_GAP: f32 = 10.;
/// Where a message's text starts, from the row's left edge.
pub(super) const RAIL: f32 = 16. + AVATAR + RAIL_GAP;

/// A reaction as a pill: the emoji and its count on a 22px line inside a
/// hairline, the reader's own in the accent wash. Without a count it is
/// the "add one" chip that ends the row.
fn reaction_pill(
    key: String,
    emoji: &str,
    count: Option<i64>,
    label: &str,
    mine: bool,
    on_press: Option<u32>,
) -> wire::Node {
    let p = native::palette();
    let mut parts = vec![native::nowrap(native::text_size(
        native::text(format!("{key}/emoji"), emoji),
        13.,
    ))];
    if let Some(count) = count {
        parts.push(native::nowrap(native::weighted(
            native::colored(
                native::text_size(
                    native::text(format!("{key}/count"), count.to_string()),
                    native::type_scale::SECONDARY as f32,
                ),
                if mine { p.accent_foreground } else { p.muted },
            ),
            wire::Weight::Medium,
        )));
    }
    let mut button = native::button_child(
        key.clone(),
        native::spaced(native::centered_row(format!("{key}/label"), parts), 4.),
        on_press,
        wire::ButtonPreset::Subtle,
    );
    if let wire::Node::Button {
        checked,
        label: accessible,
        description,
        padding,
        height,
        ..
    } = &mut button
    {
        *checked = Some(mine);
        *accessible = Some(label.into());
        *description = Some(emoji.into());
        *height = Some(wire::Length::Fixed(22.));
        *padding = Some(wire::Edges {
            top: 0.,
            right: 8.,
            bottom: 0.,
            left: 6.,
        });
    }
    let mut pill = native::container(format!("{key}/pill"), button);
    if let wire::Node::Container {
        border,
        background,
        width,
        ..
    } = &mut pill
    {
        *border = Some(wire::Border {
            color: Some(native::rgba(if mine { p.accent } else { p.border })),
            width: Some(1.),
            radius: Some([native::radius::PILL as f32; 4]),
        });
        *background = Some(wire::Background::Color(native::rgba(if mine {
            p.accent_soft
        } else {
            p.surface
        })));
        *width = Some(wire::Length::Shrink);
    }
    pill
}

/// The way into a message's thread: the reply count in the accent, and
/// beside it the invitation, on one quiet row.
fn reply_link(key: String, replies: i64, open: Message) -> wire::Node {
    let p = native::palette();
    let content = native::spaced(
        native::centered_row(
            format!("{key}/row"),
            [
                native::nowrap(native::weighted(
                    native::colored(
                        native::text(
                            format!("{key}/count"),
                            crate::host::plural(replies, "reply", "replies"),
                        ),
                        p.link,
                    ),
                    wire::Weight::Medium,
                )),
                native::nowrap(native::caption(format!("{key}/hint"), "View thread ›")),
            ],
        ),
        8.,
    );
    let mut button = native::button_child(
        key,
        content,
        Some(slots::message(open)),
        wire::ButtonPreset::Subtle,
    );
    if let wire::Node::Button {
        label,
        padding,
        height,
        ..
    } = &mut button
    {
        *label = Some("Open thread".into());
        *height = Some(wire::Length::Fixed(24.));
        *padding = Some(wire::Edges {
            top: 0.,
            right: 8.,
            bottom: 0.,
            left: 6.,
        });
    }
    native::sized(button, Some(wire::Length::Shrink), None)
}

impl ChatView {
    /// One message: the avatar rail, then the byline and body. A chosen or
    /// ranged row wears a wash instead of a caption.
    pub(super) fn message_card(
        &self,
        message: &crate::host::ChatMessage,
        surface: CopySurface,
        plate: RowPlate,
    ) -> wire::Node {
        let key = format!("message/{surface:?}/{}", message.view_key);
        let p = native::palette();
        let wash = match plate {
            RowPlate::Plain => None,
            RowPlate::Selected => Some(p.accent_soft),
            RowPlate::Ranged => Some(p.surface_raised),
        };
        let rail = if message.show_author {
            self.principal_avatar(
                format!("{key}/avatar"),
                message.initial.clone(),
                message.avatar_kind != "human",
            )
        } else {
            native::space(
                Some(wire::Length::Fixed(AVATAR)),
                Some(wire::Length::Fixed(4.)),
            )
        };
        let contents = self.message_contents(format!("{key}/contents"), message, surface);
        let mut row = native::row(format!("{key}/row"), [rail, contents]);
        if let wire::Node::Linear {
            spacing,
            padding,
            align,
            ..
        } = &mut row
        {
            *spacing = Some(RAIL_GAP);
            // a new author opens with air above; a continuation sits close
            *padding = Some(wire::Edges {
                top: if message.show_author { 10. } else { 3. },
                right: 16.,
                bottom: 3.,
                left: 16.,
            });
            *align = Some(wire::AlignX::Left);
        }
        let mut card = native::container(key, row);
        if let wire::Node::Container {
            background, border, ..
        } = &mut card
        {
            *background = wash.map(|color| wire::Background::Color(native::rgba(color)));
            *border = Some(wire::Border {
                color: None,
                width: None,
                radius: Some([native::radius::CONTROL as f32; 4]),
            });
        }
        card
    }
    pub(super) fn message_contents(
        &self,
        key: String,
        message: &crate::host::ChatMessage,
        surface: CopySurface,
    ) -> wire::Node {
        use ducktape_view_guest::slots;
        let mut children = Vec::new();
        if message.show_author {
            let p = native::palette();
            let mut header = vec![native::nowrap(native::strong(
                format!("{key}/author"),
                &message.author,
            ))];
            if message.avatar_kind == "agent" {
                header.push(native::badge(format!("{key}/agent"), "Agent", Tone::Agent));
            }
            if message.height > 0 {
                header.push(native::nowrap(native::colored(
                    native::text_size(
                        native::mono(
                            format!("{key}/height"),
                            crate::host::height_label_short(message.height),
                        ),
                        native::type_scale::CAPTION as f32,
                    ),
                    p.faint,
                )));
            }
            children.push(native::spaced(
                native::centered_row(format!("{key}/header"), header),
                6.,
            ));
        }
        children.push(wire::Node::MouseArea {
            key: format!("{key}/select"),
            on_press: Some(slots::message(Message::PressMessage(message.seq, surface))),
            on_release: None,
            on_double_click: None,
            on_right_press: None,
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: None,
            on_exit: None,
            on_move: None,
            on_press_at: None,
            on_scroll: None,
            content: Box::new(Self::message_body(
                format!("{key}/body"),
                &message.blocks,
                Some(slots::handler::<String, Message>(Box::new(|link| {
                    Some(Message::OpenMessageLink(link))
                }))),
            )),
        });
        if message.edited {
            children.push(native::caption(format!("{key}/edited"), "edited"));
        }
        let run = crate::host::run_of_message(&message.id);
        if !run.is_empty() {
            children.push(native::button(
                format!("{key}/run"),
                "View run",
                Some(slots::message(Message::OpenRun(run))),
                wire::ButtonPreset::Secondary,
            ));
        }
        let mut reactions = Vec::new();
        for reaction in &message.reactions {
            let event = if reaction.reacted_by_me {
                Message::RemoveReactionAt(message.seq, reaction.emoji.clone())
            } else {
                Message::AddReactionAt(message.seq, reaction.emoji.clone())
            };
            let label = if reaction.reacted_by_me {
                "Remove reaction"
            } else {
                "Add reaction"
            };
            reactions.push(reaction_pill(
                format!("{key}/reaction/{}", reaction.emoji),
                &reaction.emoji,
                Some(reaction.count),
                label,
                reaction.reacted_by_me,
                (!self.active_channel_archived).then(|| slots::message(event)),
            ));
        }
        // a row of reactions ends with the way to add one more
        if !reactions.is_empty() {
            let open = match surface {
                CopySurface::Thread => Message::OpenThreadMessageReactions(
                    message.seq,
                    message.body.clone(),
                    message.rev,
                ),
                CopySurface::Timeline | CopySurface::Nowhere => Message::OpenMessageReactions(
                    message.seq,
                    message.body.clone(),
                    message.rev,
                ),
            };
            reactions.push(reaction_pill(
                format!("{key}/reaction/add"),
                "😀",
                None,
                "Add reaction",
                false,
                (!self.active_channel_archived).then(|| slots::message(open)),
            ));
            children.push(native::spaced(
                native::wrapped_row(format!("{key}/reactions"), reactions),
                4.,
            ));
        }
        // in the timeline the count is the way into the thread; in the
        // thread itself it is the rule between the root and its replies
        let in_thread = surface == CopySurface::Thread;
        match (message.reply_count > 0, in_thread) {
            (false, _) => {}
            (true, false) => children.push(reply_link(
                format!("{key}/thread"),
                message.reply_count,
                Message::OpenThreadFor(message.seq),
            )),
            (true, true) => children.push(native::spaced(
                native::centered_row(
                    format!("{key}/thread"),
                    [
                        native::nowrap(native::caption(
                            format!("{key}/thread/count"),
                            crate::host::plural(message.reply_count, "reply", "replies"),
                        )),
                        native::container(
                            format!("{key}/thread/line"),
                            native::divider(format!("{key}/thread/rule")),
                        ),
                    ],
                ),
                8.,
            )),
        }
        if message.pending {
            children.push(native::caption(format!("{key}/pending"), &message.meta));
        }
        native::spaced(native::column(key, children), 3.)
    }
    pub(super) fn message_body(
        key: String,
        blocks: &[crate::host::ChatBlock],
        on_link: Option<u32>,
    ) -> wire::Node {
        let p = native::palette();
        let mut children = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let scope = format!("{key}/block/{index}");
            let content = match block.kind.as_str() {
                "divider" => native::divider(scope),
                "attachment" => Self::attachment_card(scope, block),
                "code" => {
                    let mut children = Vec::new();
                    if !block.lang.is_empty() {
                        children.push(native::caption(format!("{scope}/language"), &block.lang));
                    }
                    children.push(native::text_options(
                        native::mono(format!("{scope}/code"), &block.text),
                        wire::TextOptions {
                            wrapping: Some(wire::Wrapping::WordOrGlyph),
                            ..Default::default()
                        },
                    ));
                    let mut code = native::container(
                        scope.clone(),
                        native::spaced(native::column(format!("{scope}/code-lines"), children), 4.),
                    );
                    if let wire::Node::Container {
                        background,
                        border,
                        padding,
                        ..
                    } = &mut code
                    {
                        *background = Some(wire::Background::Color(native::rgba(p.surface)));
                        *border = Some(wire::Border {
                            color: Some(native::rgba(p.border)),
                            width: Some(1.),
                            radius: Some([native::radius::CONTROL as f32; 4]),
                        });
                        *padding = Some(wire::Edges::all(10.));
                    }
                    code
                }
                "quote" | "paragraph" => {
                    let text = if block.rich {
                        Self::rich_line(format!("{scope}/text"), block, on_link)
                    } else {
                        native::wrapping(native::text(format!("{scope}/text"), &block.text))
                    };
                    if block.kind == "quote" {
                        let mut quote = native::row(
                            scope.clone(),
                            [
                                native::vertical_divider(format!("{scope}/bar")),
                                native::colored(text, p.muted),
                            ],
                        );
                        if let wire::Node::Linear { spacing, .. } = &mut quote {
                            *spacing = Some(10.);
                        }
                        quote
                    } else {
                        text
                    }
                }
                _ => continue,
            };
            children.push(content);
        }
        native::spaced(native::column(key, children), 6.)
    }
    /// A file that came with the message: its name over what it is, in a
    /// bordered plate that opens it in Files. Slack's file card, one line.
    fn attachment_card(key: String, block: &crate::host::ChatBlock) -> wire::Node {
        let p = native::palette();
        let kind = attachment_kind(&block.text);
        let content = native::spaced(
            native::centered_row(
                format!("{key}/row"),
                [
                    native::text(format!("{key}/glyph"), "📄"),
                    native::spaced(
                        native::column(
                            format!("{key}/name"),
                            [
                                native::nowrap(native::strong(format!("{key}/title"), &block.text)),
                                native::nowrap(native::caption(format!("{key}/kind"), kind)),
                            ],
                        ),
                        1.,
                    ),
                ],
            ),
            10.,
        );
        let action = Some(slots::message(Message::OpenMessageLink(block.link.clone())));
        let mut card = native::button_child(
            format!("{key}/card"),
            content,
            action,
            wire::ButtonPreset::Secondary,
        );
        if let wire::Node::Button {
            label,
            padding,
            width,
            style,
            ..
        } = &mut card
        {
            *label = Some(format!("Open {}", block.text));
            *padding = Some(wire::Edges {
                top: 8.,
                right: 14.,
                bottom: 8.,
                left: 12.,
            });
            *width = Some(wire::Length::Shrink);
            style.active.background = Some(native::rgba(p.surface));
            style.active.border = Some(wire::Border {
                color: Some(native::rgba(p.border)),
                width: Some(1.),
                radius: Some([native::radius::CARD as f32; 4]),
            });
        }
        // a column stretches its children; a row lets the card hug its name
        native::row(key, [card])
    }
    pub(super) fn rich_line(
        key: String,
        block: &crate::host::ChatBlock,
        on_link: Option<u32>,
    ) -> wire::Node {
        let p = native::palette();
        let mut spans = Vec::new();
        for part in &block.spans {
            for (content, link, weight, italic) in [
                (
                    &part.mention,
                    Some(&part.mention_link),
                    wire::Weight::Medium,
                    false,
                ),
                (
                    &part.link_text,
                    Some(&part.link),
                    wire::Weight::Medium,
                    false,
                ),
                (&part.bold_italic, None, wire::Weight::Bold, true),
                (&part.bold, None, wire::Weight::Bold, false),
                (&part.italic, None, wire::Weight::Normal, true),
                (&part.plain, None, wire::Weight::Normal, false),
            ] {
                if content.is_empty() {
                    continue;
                }
                let decorated = weight != wire::Weight::Normal || italic;
                spans.push(wire::RichSpan {
                    content: content.clone(),
                    link: link.cloned(),
                    underline: link.is_some(),
                    color: link.is_some().then_some(native::rgba(p.link)),
                    font: decorated.then_some(wire::NamedFont {
                        family: wire::FontFamily::SansSerif,
                        weight,
                        stretch: wire::FontStretch::Normal,
                        style: if italic {
                            wire::FontStyle::Italic
                        } else {
                            wire::FontStyle::Normal
                        },
                    }),
                    ..Default::default()
                });
            }
        }
        wire::Node::RichText {
            key,
            spans,
            on_link,
            options: wire::TextOptions {
                wrapping: Some(wire::Wrapping::WordOrGlyph),
                ..Default::default()
            },
            size: None,
            color: None,
            font: Default::default(),
            width: Some(wire::Length::Fill),
            align_x: None,
        }
    }
    /// The avatar beside a message: the kit's plate grown to the message
    /// rail's 28px, a rounded square rather than a pill, initials to match.
    pub(super) fn principal_avatar(
        &self,
        key: String,
        initials: String,
        agent: bool,
    ) -> wire::Node {
        let tone = if agent { Tone::Agent } else { Tone::Neutral };
        let mut avatar = native::avatar(key, initials, tone);
        if let wire::Node::Container {
            width,
            height,
            border,
            content,
            ..
        } = &mut avatar
        {
            *width = Some(wire::Length::Fixed(AVATAR));
            *height = Some(wire::Length::Fixed(AVATAR));
            *border = Some(wire::Border {
                color: None,
                width: None,
                radius: Some([native::radius::CARD as f32; 4]),
            });
            if let wire::Node::Text { size, .. } = content.as_mut() {
                *size = Some(11.5);
            }
        }
        avatar
    }

    pub(super) fn active_dm_avatar(&self, key: String) -> wire::Node {
        self.principal_avatar(
            key,
            self.active_dm.initials.clone(),
            self.active_dm.is_agent,
        )
    }

    pub(super) fn archived_badge(&self, key: String) -> wire::Node {
        native::badge(key, "Archived", Tone::Neutral)
    }
    pub(super) fn private_badge(&self, key: String) -> wire::Node {
        native::badge(key, "Members only", Tone::Neutral)
    }

    pub(super) fn huddle_controls(
        &self,
        key: String,
        leave: impl Fn() -> Message + Clone + 'static,
        show: impl Fn() -> Message + Clone + 'static,
    ) -> wire::Node {
        let elapsed = crate::host::mmss(self.huddle_now - self.huddle_joined_at);
        let mut children = vec![native::badge(
            format!("{key}/live"),
            format!("Live {elapsed}"),
            Tone::Success,
        )];
        if self.call_muted {
            children.push(native::badge(
                format!("{key}/muted"),
                "Muted",
                Tone::Neutral,
            ));
        }
        // The live badge already says "huddle"; the controls stay one word
        // each so the header fits beside a thread and a details pane.
        let mut show = native::button(
            format!("{key}/show"),
            "Show",
            Some(slots::message(show())),
            wire::ButtonPreset::Subtle,
        );
        let mut leave = native::button(
            format!("{key}/leave"),
            "Leave",
            Some(slots::message(leave())),
            wire::ButtonPreset::Subtle,
        );
        for (button, label) in [(&mut show, "Show huddle"), (&mut leave, "Leave huddle")] {
            if let wire::Node::Button { label: name, .. } = button {
                *name = Some(label.into());
            }
        }
        children.extend([show, leave]);
        native::sized(
            native::spaced(native::centered_row(&key, children), 6.),
            Some(wire::Length::Shrink),
            None,
        )
    }

    pub(super) fn start_huddle(
        &self,
        key: String,
        join: impl Fn() -> Message + Clone + 'static,
    ) -> wire::Node {
        // one word in the header; the full name is what a reader hears
        let mut button = native::button(
            key,
            "Huddle",
            Some(slots::message(join())),
            wire::ButtonPreset::Subtle,
        );
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some("Start a huddle".into());
        }
        button
    }

    pub(super) fn disconnected(&self, key: String) -> wire::Node {
        native::empty_state(
            key,
            "Not connected",
            "Choose a network from the sidebar to reconnect.",
        )
    }

    /// What stands where the composer would: the room is archived, and the
    /// way to reopen it is right there.
    pub(super) fn archived_notice(&self, key: String) -> wire::Node {
        let mut reopen = native::button(
            format!("{key}/unarchive"),
            "Unarchive",
            (!self.busy).then(|| slots::message(Message::UnarchiveChannelSubmit)),
            wire::ButtonPreset::Secondary,
        );
        if let wire::Node::Button { label, .. } = &mut reopen {
            *label = Some("Unarchive channel".into());
        }
        native::notice(
            key.clone(),
            native::spaced(
                native::centered_row(
                    format!("{key}/row"),
                    [
                        native::sized(
                            native::wrapping(native::text(
                                format!("{key}/text"),
                                "This channel is archived. It keeps its history and takes no new messages.",
                            )),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        reopen,
                    ],
                ),
                12.,
            ),
            Tone::Neutral,
        )
    }

    pub(super) fn private_notice(&self, key: String) -> wire::Node {
        native::notice(
            key.clone(),
            native::wrapping(native::text(
                format!("{key}/text"),
                "This channel is members-only and your key is not on its roster. Ask a member to add your key from Channel details.",
            )),
            Tone::Warning,
        )
    }

    pub(super) fn name_label(&self, key: String) -> wire::Node {
        native::label(key, "Name")
    }
    pub(super) fn members_label(&self, key: String) -> wire::Node {
        native::label(key, "Members")
    }
}

/// What a file is, from its extension: the caption under its name.
fn attachment_kind(name: &str) -> String {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_uppercase());
    match extension {
        Some(ext) if !ext.is_empty() && ext.len() <= 5 => format!("{ext} file"),
        _ => "File".into(),
    }
}
