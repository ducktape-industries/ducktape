use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

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
            children.push(native::badge(
                format!("{key}/huddle"),
                format!("Huddle {}", channel.huddle_count),
                Tone::Success,
            ));
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
            children.push(native::badge(format!("{key}/unread"), "Unread", Tone::Accent));
        }
        let action = if self.busy {
            None
        } else {
            Some(slots::message(choose(channel.id)))
        };
        let content = native::spaced(native::centered_row(format!("{key}/row"), children), 6.);
        let mut button = native::list_row(key, content, selected, action);
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some(channel.name);
        }
        button
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
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some(hit.text);
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
