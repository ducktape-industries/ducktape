use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

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
            native::space(Some(wire::Length::Fixed(28.)), Some(wire::Length::Fixed(4.)))
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
            *spacing = Some(10.);
            *padding = Some(wire::Edges {
                top: if message.show_author { 8. } else { 1. },
                right: 16.,
                bottom: 1.,
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
            let mut header = vec![native::nowrap(native::strong(
                format!("{key}/author"),
                &message.author,
            ))];
            if message.avatar_kind == "agent" {
                header.push(native::badge(format!("{key}/agent"), "Agent", Tone::Agent));
            }
            if message.height > 0 {
                header.push(native::nowrap(native::caption(
                    format!("{key}/height"),
                    crate::host::height_label_short(message.height),
                )));
            }
            children.push(native::centered_row(format!("{key}/header"), header));
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
            let mut button = native::button_child(
                format!("{key}/reaction/{}", reaction.emoji),
                native::spaced(
                    native::row(
                        format!("{key}/reaction/{}/label", reaction.emoji),
                        [
                            native::nowrap(native::text(
                                format!("{key}/reaction/{}/emoji", reaction.emoji),
                                &reaction.emoji,
                            )),
                            native::nowrap(native::caption(
                                format!("{key}/reaction/{}/count", reaction.emoji),
                                reaction.count.to_string(),
                            )),
                        ],
                    ),
                    4.,
                ),
                Some(slots::message(event)),
                wire::ButtonPreset::Background,
            );
            if let wire::Node::Button {
                checked,
                label,
                description,
                padding,
                ..
            } = &mut button
            {
                *checked = Some(reaction.reacted_by_me);
                *label = Some(
                    if reaction.reacted_by_me {
                        "Remove reaction"
                    } else {
                        "Add reaction"
                    }
                    .into(),
                );
                *description = Some(reaction.emoji.clone());
                *padding = Some(wire::Edges {
                    top: 2.,
                    right: 8.,
                    bottom: 2.,
                    left: 8.,
                });
            }
            reactions.push(button);
        }
        if !reactions.is_empty() {
            children.push(native::spaced(
                native::wrapped_row(format!("{key}/reactions"), reactions),
                4.,
            ));
        }
        if message.reply_count > 0 {
            let mut button = native::button(
                format!("{key}/thread"),
                crate::host::plural(message.reply_count, "reply", "replies"),
                Some(slots::message(Message::OpenThreadFor(message.seq))),
                wire::ButtonPreset::Text,
            );
            if let wire::Node::Button { label, .. } = &mut button {
                *label = Some("Open thread".into());
            }
            children.push(native::row(format!("{key}/thread-row"), [button]));
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
                        scope,
                        native::spaced(native::column(format!("{key}/code-lines"), children), 4.),
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
    pub(super) fn principal_avatar(
        &self,
        key: String,
        initials: String,
        agent: bool,
    ) -> wire::Node {
        let tone = if agent { Tone::Agent } else { Tone::Neutral };
        native::avatar(key, initials, tone)
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
        let mute = if self.call_muted { " · Muted" } else { "" };
        native::row(
            &key,
            [
                native::button(
                    format!("{key}/show"),
                    format!("Live {elapsed}{mute}"),
                    Some(slots::message(show())),
                    wire::ButtonPreset::Success,
                ),
                native::button(
                    format!("{key}/leave"),
                    "Leave huddle",
                    Some(slots::message(leave())),
                    wire::ButtonPreset::Subtle,
                ),
            ],
        )
    }

    pub(super) fn start_huddle(
        &self,
        key: String,
        join: impl Fn() -> Message + Clone + 'static,
    ) -> wire::Node {
        native::button(
            key,
            "Start a huddle",
            Some(slots::message(join())),
            wire::ButtonPreset::Subtle,
        )
    }

    pub(super) fn disconnected(&self, key: String) -> wire::Node {
        native::empty_state(
            key,
            "Not connected",
            "Choose a network from the sidebar to reconnect.",
        )
    }

    pub(super) fn empty_messages(&self, key: String) -> wire::Node {
        native::empty_state(
            key,
            "No messages yet",
            "Nobody has posted here. Send the first message below.",
        )
    }

    pub(super) fn archived_notice(&self, key: String) -> wire::Node {
        native::notice(
            key.clone(),
            native::wrapping(native::text(
                format!("{key}/text"),
                "This channel is archived. Unarchive it from Channel details to post here again.",
            )),
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
