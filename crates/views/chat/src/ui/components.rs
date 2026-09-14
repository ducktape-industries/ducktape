use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

/// What marks an unread room: an 8px accent dot at the row's end. The
/// name is already bold; the dot is what the eye catches in a long list.
pub(super) fn unread_dot(key: String) -> wire::Node {
    let mut dot = native::container(
        key,
        native::space(
            Some(wire::Length::Fixed(8.)),
            Some(wire::Length::Fixed(8.)),
        ),
    );
    if let wire::Node::Container {
        background,
        border,
        width,
        ..
    } = &mut dot
    {
        *background = Some(wire::Background::Color(native::rgba(
            native::palette().accent,
        )));
        *border = Some(wire::Border {
            color: None,
            width: None,
            radius: Some([native::radius::PILL as f32; 4]),
        });
        *width = Some(wire::Length::Shrink);
    }
    dot
}

/// A room in the list pane: a 28px row, its content centred on the row,
/// the name as its accessible label.
pub(super) fn sidebar_row(mut button: wire::Node, name: String) -> wire::Node {
    if let wire::Node::Button {
        label,
        height,
        padding,
        ..
    } = &mut button
    {
        *label = Some(name);
        *height = Some(wire::Length::Fixed(28.));
        *padding = Some(wire::Edges {
            top: 0.,
            right: 8.,
            bottom: 0.,
            left: 8.,
        });
    }
    button
}

impl ChatView {
    /// A channel in the list pane: the hash, the name, and what stands out
    /// about it. An unread room is emphasised and carries a mark.
    pub(super) fn channel_button(
        &self,
        key: String,
        choose: impl Fn(String) -> Message + Clone + 'static,
        channel: crate::host::ChatChannel,
        selected: bool,
        unread: bool,
    ) -> wire::Node {
        let p = native::palette();
        let name = if unread {
            native::strong(format!("{key}/name"), &channel.name)
        } else {
            native::text(format!("{key}/name"), &channel.name)
        };
        let mut children = vec![
            native::nowrap(native::colored(
                native::text(format!("{key}/hash"), "#"),
                p.muted,
            )),
            native::nowrap(name),
        ];
        if channel.huddle_count > 0 {
            children.push(native::nowrap(native::colored(
                native::text_size(
                    native::text(
                        format!("{key}/huddle"),
                        format!("🔊 {}", channel.huddle_count),
                    ),
                    native::type_scale::CAPTION as f32,
                ),
                p.success,
            )));
        }
        if channel.members_only {
            children.push(native::nowrap(native::caption(
                format!("{key}/members-only"),
                "Members only",
            )));
        }
        if channel.archived {
            children.push(native::nowrap(native::caption(
                format!("{key}/archived"),
                "Archived",
            )));
        }
        if unread {
            children.push(native::spacer());
            children.push(unread_dot(format!("{key}/unread")));
        }
        let action = if self.busy {
            None
        } else {
            Some(slots::message(choose(channel.id)))
        };
        let content = native::spaced(native::centered_row(format!("{key}/row"), children), 6.);
        let row = sidebar_row(
            native::list_row(key.clone(), content, selected, action),
            channel.name,
        );
        if channel.huddle.is_empty() {
            return row;
        }
        // the people in the room's huddle, under the room like a voice channel
        let mut seats = vec![row];
        for (index, seat) in channel.huddle.iter().enumerate() {
            seats.push(self.huddle_seat(format!("{key}/seat/{index}"), seat));
        }
        native::spaced(native::column(format!("{key}/with-huddle"), seats), 2.)
    }

    /// One person in a huddle, as the room list shows them: a small plate
    /// (lit while they talk), the name, and "you" (muted or not) on the
    /// reader's own seat.
    fn huddle_seat(&self, key: String, seat: &crate::host::HuddleSeat) -> wire::Node {
        let speaking = crate::host::seat_speaking(seat, self.call_speaking, &self.speaking_peers);
        let tone = match speaking {
            true => Tone::Success,
            false => Tone::Neutral,
        };
        let mut children = vec![
            native::avatar(format!("{key}/avatar"), seat.initials.clone(), tone),
            native::nowrap(native::secondary(format!("{key}/name"), &seat.label)),
        ];
        let mine = match (seat.is_you, self.call_muted) {
            (true, true) => "you · muted",
            (true, false) => "you",
            (false, _) => "",
        };
        if !mine.is_empty() {
            children.push(native::nowrap(native::caption(format!("{key}/you"), mine)));
        }
        let mut row = native::centered_row(key, children);
        if let wire::Node::Linear {
            spacing, padding, ..
        } = &mut row
        {
            *spacing = Some(8.);
            *padding = Some(wire::Edges {
                top: 2.,
                right: 8.,
                bottom: 2.,
                left: 28.,
            });
        }
        row
    }

    pub(super) fn loading_messages(&self, key: String) -> wire::Node {
        native::padded(
            native::column(
                key.clone(),
                [native::caption(format!("{key}/text"), "Loading messages…")],
            ),
            wire::Edges::all(16.),
        )
    }

    pub(super) fn search_result(
        &self,
        key: String,
        open: impl Fn(String, i64, i64) -> Message + Clone + 'static,
        hit: crate::host::ChatSearchHit,
    ) -> wire::Node {
        // the room by its name; the id is what the node keys it by, not a reading
        let room = self
            .rooms
            .iter()
            .find(|room| room.channel.id == hit.channel_id)
            .map_or_else(
                || format!("#{}", hit.channel_id),
                |room| format!("#{}", room.channel.name),
            );
        let action = slots::message(open(hit.channel_id, hit.root_seq, hit.seq));
        let content = native::spaced(
            native::column(
                format!("{key}/content"),
                [
                    native::spaced(
                        native::centered_row(
                            format!("{key}/byline"),
                            [
                                native::nowrap(native::strong(format!("{key}/author"), hit.author)),
                                native::nowrap(native::caption(format!("{key}/room"), room)),
                                native::nowrap(native::caption(format!("{key}/meta"), hit.meta)),
                            ],
                        ),
                        6.,
                    ),
                    native::wrapping(native::secondary(format!("{key}/text"), hit.text.clone())),
                ],
            ),
            2.,
        );
        let mut button = native::list_row(key, content, false, Some(action));
        if let wire::Node::Button { label, padding, .. } = &mut button {
            *label = Some(hit.text);
            // two lines a hit: more air than a one-line room row
            *padding = Some(wire::Edges {
                top: 6.,
                right: 8.,
                bottom: 6.,
                left: 8.,
            });
        }
        button
    }

    pub(super) fn composer_gate(&self, key: String) -> wire::Node {
        match self.post_refusal.as_str() {
            "channel_archived" => self.archived_notice(key),
            "members_only" => self.private_notice(key),
            _ => native::column(key, []),
        }
    }

    pub(super) fn member_row(
        &self,
        key: String,
        remove: impl Fn(String) -> Message + Clone + 'static,
        member: crate::host::ChatMember,
    ) -> wire::Node {
        let action = if self.busy {
            None
        } else {
            Some(slots::message(remove(member.key)))
        };
        let mut button = native::button(
            format!("{key}/remove"),
            "Remove",
            action,
            wire::ButtonPreset::Subtle,
        );
        if let wire::Node::Button {
            description, label, ..
        } = &mut button
        {
            *label = Some("Remove member".into());
            *description = Some(member.label.clone());
        }
        native::centered_row(
            &key,
            [
                native::wrapping(native::text(format!("{key}/name"), member.label)),
                button,
            ],
        )
    }

    pub(super) fn live_run_card(
        &self,
        key: String,
        stop: impl Fn(String) -> Message + Clone + 'static,
        open: impl Fn(String) -> Message + Clone + 'static,
        run: crate::host::LiveRunHint,
    ) -> wire::Node {
        native::card(
            key.clone(),
            native::column(
                format!("{key}/body"),
                [
                    native::centered_row(
                        format!("{key}/byline"),
                        [
                            native::badge(format!("{key}/kind"), "Agent", Tone::Agent),
                            native::nowrap(native::strong(format!("{key}/agent"), run.agent)),
                        ],
                    ),
                    native::wrapping(native::secondary(format!("{key}/status"), run.status)),
                    native::row(
                        format!("{key}/actions"),
                        [
                            native::button(
                                format!("{key}/open"),
                                "View run",
                                Some(slots::message(open(run.dispatch_id))),
                                wire::ButtonPreset::Secondary,
                            ),
                            native::button(
                                format!("{key}/stop"),
                                "Stop",
                                Some(slots::message(stop(run.run_id))),
                                wire::ButtonPreset::Subtle,
                            ),
                        ],
                    ),
                ],
            ),
        )
    }
}
