use super::*;
use ducktape_view_guest::kit::Tone;
use ducktape_view_guest::slots;

impl ChatView {
    /// A direct message in the list pane: the peer's avatar and name.
    pub(super) fn direct_message(
        &self,
        key: String,
        choose: impl Fn(String) -> Message + Clone + 'static,
        peer: crate::host::DmPeer,
        selected: bool,
        unread: bool,
    ) -> wire::Node {
        let name = if unread {
            native::strong(format!("{key}/name"), &peer.name)
        } else {
            native::text(format!("{key}/name"), &peer.name)
        };
        // the kit's 24px plate: a list row is 28px, the message rail's 28px
        // avatar belongs beside a message
        let tone = if peer.is_agent {
            Tone::Agent
        } else {
            Tone::Neutral
        };
        let mut children = vec![
            native::avatar(format!("{key}/avatar"), peer.initials.clone(), tone),
            native::nowrap(name),
        ];
        if peer.is_agent {
            children.push(native::badge(format!("{key}/agent"), "Agent", Tone::Agent));
        }
        if unread {
            children.push(native::spacer());
            children.push(super::components::unread_dot(format!("{key}/unread")));
        }
        let action = if self.busy {
            None
        } else {
            Some(slots::message(choose(peer.key)))
        };
        let content = native::spaced(native::centered_row(format!("{key}/row"), children), 8.);
        super::components::sidebar_row(native::list_row(key, content, selected, action), peer.name)
    }

    pub(super) fn direct_message_header(&self, key: String) -> wire::Node {
        native::centered_row(
            &key,
            [
                self.active_dm_avatar(format!("{key}/avatar")),
                native::heading(format!("{key}/name"), self.active_dm.name.clone()),
            ],
        )
    }
}
