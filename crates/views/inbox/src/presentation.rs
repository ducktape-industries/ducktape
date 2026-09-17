//! The inbox's tree: a head that says how much is unread and offers the one
//! write this screen has, then the newest notifications — each a sentence
//! about what happened, the source's own text under it, and a door to the tab
//! that owns the object.

use crate::{InboxView, Message, host};
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{ButtonPreset, Length, Node},
};

impl InboxView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let mut body = vec![self.head()];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "inbox/host-error",
                kit::wrapping(kit::text(
                    "inbox/host-error-text",
                    format!("Could not read the session: {}", self.host_error),
                )),
                Tone::Danger,
            ));
        }
        if !self.connected {
            body.push(kit::empty_state(
                "inbox/disconnected",
                "Not connected",
                "Choose a network from the sidebar to see what it sent you.",
            ));
            return kit::page("inbox/content", body);
        }
        if self.account.is_empty() {
            body.push(kit::empty_state(
                "inbox/no-account",
                "No account on this device",
                "Notifications are addressed to an account. Create or join one in Settings.",
            ));
            return kit::page("inbox/content", body);
        }
        if !self.error.is_empty() {
            body.push(kit::notice(
                "inbox/error",
                kit::wrapping(kit::text("inbox/error-text", self.error.clone())),
                Tone::Danger,
            ));
        }
        body.push(self.list());
        kit::page("inbox/content", body)
    }

    fn head(&self) -> Node {
        let mut cells = vec![kit::sized(
            kit::title("inbox/title", "Inbox"),
            Some(Length::Fill),
            None,
        )];
        if self.unread > 0 {
            cells.push(kit::badge(
                "inbox/unread",
                format!("{} unread", self.unread),
                Tone::Accent,
            ));
        }
        let can_mark = self.unread > 0 && self.head > 0 && !self.marking;
        cells.push(kit::button(
            "inbox/mark-read",
            match self.marking {
                true => "Marking…",
                false => "Mark all read",
            },
            can_mark.then(|| slots::message(Message::MarkAllRead)),
            ButtonPreset::Secondary,
        ));
        kit::spaced(kit::centered_row("inbox/head", cells), 8.)
    }

    fn list(&self) -> Node {
        if self.reading {
            return kit::secondary("inbox/reading", "Reading your notifications…");
        }
        if self.rows.is_empty() {
            return kit::empty_state(
                "inbox/empty",
                "Nothing new",
                "Mentions, assignments and results addressed to you land here.",
            );
        }
        kit::scroll(
            "inbox/list",
            kit::column(
                "inbox/rows",
                self.rows.iter().map(|row| self.row(row)).collect::<Vec<_>>(),
            ),
        )
    }

    fn row(&self, row: &host::Row) -> Node {
        let key = format!("inbox/row/{}", row.seq);
        let mut line = vec![kit::sized(
            kit::nowrap(kit::strong(format!("{key}/title"), row.title.clone())),
            Some(Length::Fill),
            None,
        )];
        if !row.read {
            line.push(kit::badge(format!("{key}/new"), "New", Tone::Accent));
        }
        let entry = kit::spaced(
            kit::column(
                key.clone(),
                [
                    kit::spaced(kit::centered_row(format!("{key}/line"), line), 8.),
                    kit::nowrap(kit::secondary(
                        format!("{key}/detail"),
                        row.detail.clone(),
                    )),
                ],
            ),
            2.,
        );
        // A row with no address opens nothing — the source is gone, or this
        // app has no tab for it — and says so by not being pressable.
        let door = (!row.link.is_empty()).then(|| slots::message(Message::Open(row.link.clone())));
        kit::list_row(format!("{key}/press"), entry, false, door)
    }
}
