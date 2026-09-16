//! The palette's tree: nothing at all while it is closed, and while it is
//! open the whole window — a scrim that dismisses, and the card the reader
//! types in.
//!
//! The card is this view's, scrim included: what a palette looks like, where
//! it sits and what closes it are the same swap as what it finds.

use crate::{Message, PaletteView, host};
use ducktape_view_guest::{
    kit, slots,
    wire::{self, Length, Node},
};

/// How far below the window's top the card hangs.
const DROP: f32 = 96.;
/// The card's width, and the height its results may take.
const CARD: f32 = 560.;
const RESULTS: f32 = 420.;

impl PaletteView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        // A CLOSED PALETTE IS AN EMPTY TREE, and an empty tree is what tells
        // the app there is no overlay to stack: nothing is drawn, nothing is
        // clicked through, and nothing is read.
        if !self.open {
            return kit::column("palette/closed", Vec::new());
        }
        let scrim = kit::button_child(
            "palette/scrim",
            kit::sized(
                kit::column("palette/scrim-fill", Vec::new()),
                Some(Length::Fill),
                Some(Length::Fill),
            ),
            Some(slots::message(Message::Dismiss)),
            wire::ButtonPreset::Subtle,
        );
        let card = kit::aligned(
            kit::column(
                "palette/drop",
                [kit::space(None, Some(Length::Fixed(DROP))), self.card()],
            ),
            wire::AlignX::Center,
        );
        Node::Stack {
            key: "palette/overlay".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            padding: None,
            background: None,
            border: None,
            clip: false,
            under: 0,
            children: vec![scrim, card],
        }
    }

    fn card(&self) -> Node {
        let typed = slots::handler(Box::new(|text: String| Some(Message::DraftChanged(text))));
        let mut body = vec![kit::input(
            "palette/input",
            "Search messages and pages",
            self.draft.clone(),
            typed,
            None,
        )];
        if let Some(note) = self.note() {
            body.push(kit::secondary("palette/note", note));
        }
        body.push(self.hits());
        kit::sized(
            kit::card("palette/card", kit::spaced(kit::column("palette/body", body), 6.)),
            Some(Length::Fixed(CARD)),
            None,
        )
    }

    /// The one line that says what the field is doing, or nothing when the
    /// hits speak for themselves.
    fn note(&self) -> Option<String> {
        if !self.connected {
            return Some("Not connected to a network.".into());
        }
        if self.searching {
            return Some("Searching…".into());
        }
        if !self.error.is_empty() {
            return Some(self.error.clone());
        }
        let asked = !self.query.is_empty();
        let empty = self.chat.is_empty() && self.pages.is_empty();
        match asked && empty {
            true => Some("No messages or pages matched.".into()),
            false => None,
        }
    }

    fn hits(&self) -> Node {
        let mut rows = Vec::new();
        for hit in &self.chat {
            rows.push(self.row(
                format!("palette/chat/{}/{}", hit.channel_id, hit.seq),
                hit.author.clone(),
                hit.text.clone(),
                host::chat_link(hit, &self.chain),
            ));
        }
        for hit in &self.pages {
            rows.push(self.row(
                format!("palette/page/{}/{}", hit.page_id, hit.block_id),
                hit.title.clone(),
                hit.text.clone(),
                host::page_link(hit, &self.chain),
            ));
        }
        if rows.is_empty() {
            return kit::column("palette/hits", Vec::new());
        }
        kit::sized(
            kit::scroll("palette/hits", kit::column("palette/rows", rows)),
            Some(Length::Fill),
            Some(Length::Fixed(RESULTS)),
        )
    }

    fn row(&self, key: String, name: String, detail: String, link: String) -> Node {
        let entry = kit::spaced(
            kit::column(
                key.clone(),
                [
                    kit::nowrap(kit::strong(format!("{key}/name"), name)),
                    kit::nowrap(kit::secondary(format!("{key}/detail"), detail)),
                ],
            ),
            2.,
        );
        kit::list_row(
            format!("{key}/press"),
            entry,
            false,
            Some(slots::message(Message::Open(link))),
        )
    }
}
