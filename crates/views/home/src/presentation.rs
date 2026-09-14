//! The dashboard's tree: a head, then a rail of cards — each one a reading
//! of one plane and, where the `duck://` grammar names one, a door to its
//! tab. Same ink as the sibling views: a title row, cards over a hairline,
//! mono for keys and heights, a badge for a state.

use crate::{HomeView, Message, host};
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{ButtonPreset, Length, Node},
};

/// The density a card's row is built on.
const LIST_ROW: f32 = 28.;

impl HomeView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let mut head = vec![kit::sized(
            kit::title("home/title", "Home"),
            Some(Length::Fill),
            None,
        )];
        if self.connected && !self.facts.phase.is_empty() {
            head.push(kit::badge(
                "home/phase",
                &self.facts.phase,
                Tone::Success,
            ));
        }
        let mut body = vec![kit::centered_row("home/head", head)];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "home/error",
                kit::wrapping(kit::text(
                    "home/error-text",
                    format!("Could not read the session: {}", self.host_error),
                )),
                Tone::Danger,
            ));
        }
        if !self.connected {
            body.push(kit::empty_state(
                "home/disconnected",
                "Not connected",
                "Choose a network from the sidebar to see it here.",
            ));
            return kit::page("home/content", body);
        }
        let cards = [
            self.node_card(),
            self.members_card(),
            self.rooms_card(),
            self.runs_card(),
            self.proposals_card(),
            self.files_card(),
        ];
        body.push(kit::scroll(
            "home/cards",
            kit::spaced(kit::column("home/rail", cards), 16.),
        ));
        kit::page("home/content", body)
    }

    /// One card: its title over a hairline, its reading error if it has
    /// one, then its body.
    fn card(&self, name: &str, title: &str, body: Node) -> Node {
        let key = format!("home/{name}");
        let mut children = vec![
            kit::heading(format!("{key}/title"), title),
            kit::divider(format!("{key}/rule")),
        ];
        if let Some(error) = self.errors.get(name) {
            children.push(kit::notice(
                format!("{key}/error"),
                kit::wrapping(kit::secondary(
                    format!("{key}/error-text"),
                    format!("Not read: {error}"),
                )),
                Tone::Warning,
            ));
        }
        children.push(body);
        let body = kit::spaced(kit::column(format!("{key}/body"), children), 8.);
        kit::card(key, body)
    }

    fn reading(key: &str, name: &str, value: Node) -> Node {
        kit::sized(kit::kv(key, name, value), None, Some(Length::Fixed(LIST_ROW)))
    }

    fn node_card(&self) -> Node {
        let facts = &self.facts;
        let key_row = kit::centered_row(
            "home/node/key",
            [
                kit::sized(
                    kit::kv(
                        "home/node/key/reading",
                        "Node key",
                        kit::mono("home/node/key/value", host::short_label(&facts.node_key)),
                    ),
                    Some(Length::Fill),
                    None,
                ),
                kit::button(
                    "home/node/key/copy",
                    "Copy",
                    Some(slots::message(Message::CopyToClipboard(
                        facts.node_key.clone(),
                        "Node key copied".into(),
                    ))),
                    ButtonPreset::Subtle,
                ),
            ],
        );
        let readings = kit::column(
            "home/node/readings",
            [
                Self::reading(
                    "home/node/height",
                    "Height",
                    kit::mono("home/node/height/value", host::height_label(facts.height)),
                ),
                Self::reading(
                    "home/node/sync",
                    "Doing",
                    kit::text("home/node/sync/value", &facts.sync_line),
                ),
                Self::reading(
                    "home/node/chain",
                    "Network",
                    kit::mono("home/node/chain/value", self.chain_id()),
                ),
                key_row,
            ],
        );
        self.card("node", "This node", readings)
    }

    fn members_card(&self) -> Node {
        let roster = &self.roster;
        let tier = match roster.tier.is_empty() {
            true => "Not reported".to_owned(),
            false => host::capitalized(&roster.tier),
        };
        let readings = kit::column(
            "home/members/readings",
            [
                Self::reading(
                    "home/members/validators",
                    "Validators",
                    kit::mono(
                        "home/members/validators/value",
                        host::grouped_digits(roster.validators),
                    ),
                ),
                Self::reading(
                    "home/members/residents",
                    "Residents",
                    kit::mono(
                        "home/members/residents/value",
                        host::grouped_digits(roster.residents),
                    ),
                ),
                Self::reading(
                    "home/members/tier",
                    "This node",
                    kit::badge("home/members/tier/value", tier, Tone::Neutral),
                ),
            ],
        );
        self.card("members", "Members", readings)
    }

    fn rooms_card(&self) -> Node {
        let rooms = host::rooms_with_news(&self.rooms, &self.rooms_seen);
        let mut rows = Vec::new();
        for (room, moved) in rooms.iter().take(host::ROOM_ROWS) {
            let key = format!("home/room/{}", room.id);
            let mut cells = vec![kit::sized(
                kit::nowrap(kit::text(format!("{key}/name"), format!("#{}", room.name))),
                Some(Length::Fill),
                None,
            )];
            if *moved {
                cells.push(kit::badge(format!("{key}/new"), "New", Tone::Success));
            }
            cells.push(kit::mono(
                format!("{key}/head"),
                format!("{} messages", host::grouped_digits(room.head_seq)),
            ));
            let row = kit::list_row(
                key.clone(),
                kit::spaced(kit::centered_row(format!("{key}/row"), cells), 8.),
                false,
                Some(slots::message(Message::OpenRoom(room.id.clone()))),
            );
            rows.push(Self::labelled(row, format!("Open room {}", room.name)));
        }
        if rows.is_empty() {
            rows.push(kit::empty_state(
                "home/rooms/empty",
                "No rooms",
                "Nobody has opened a channel on this network yet.",
            ));
        }
        self.card(
            "rooms",
            "Rooms",
            kit::spaced(kit::column("home/rooms/list", rows), 0.),
        )
    }

    fn runs_card(&self) -> Node {
        let mut rows = Vec::new();
        for run in self.runs.iter().take(host::RUN_ROWS) {
            let key = format!("home/run/{}", run.dispatch_id);
            let tone = match run.state.as_str() {
                "accepted" => Tone::Success,
                "rejected" | "failed" => Tone::Danger,
                _ => Tone::Neutral,
            };
            let row = kit::list_row(
                key.clone(),
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            kit::sized(
                                kit::nowrap(kit::text(format!("{key}/agent"), &run.agent_id)),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::badge(
                                format!("{key}/state"),
                                host::capitalized(&run.state),
                                tone,
                            ),
                            kit::mono(
                                format!("{key}/height"),
                                host::height_label(run.dispatched_height),
                            ),
                        ],
                    ),
                    8.,
                ),
                false,
                Some(slots::message(Message::OpenRun(run.dispatch_id.clone()))),
            );
            rows.push(Self::labelled(
                row,
                format!("Open run {}", host::short_label(&run.dispatch_id)),
            ));
        }
        if rows.is_empty() {
            rows.push(kit::empty_state(
                "home/runs/empty",
                "No runs",
                "No agent has been dispatched on this network yet.",
            ));
        }
        self.card(
            "runs",
            "Agent runs",
            kit::spaced(kit::column("home/runs/list", rows), 0.),
        )
    }

    fn proposals_card(&self) -> Node {
        let mut rows = Vec::new();
        for proposal in self.proposals.iter().take(host::PROPOSAL_ROWS) {
            let key = format!("home/proposal/{}", proposal.id);
            rows.push(kit::sized(
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            kit::sized(
                                kit::nowrap(kit::text(format!("{key}/action"), &proposal.action)),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::badge(
                                format!("{key}/approvals"),
                                format!("{} approvals", proposal.approvals),
                                Tone::Neutral,
                            ),
                            kit::mono(
                                format!("{key}/deadline"),
                                format!("expires {}", host::height_label(proposal.deadline)),
                            ),
                        ],
                    ),
                    8.,
                ),
                None,
                Some(Length::Fixed(LIST_ROW)),
            ));
        }
        if rows.is_empty() {
            rows.push(kit::empty_state(
                "home/proposals/empty",
                "Nothing to decide",
                "No proposal is open on this network.",
            ));
        }
        self.card(
            "proposals",
            "Open proposals",
            kit::spaced(kit::column("home/proposals/list", rows), 0.),
        )
    }

    fn files_card(&self) -> Node {
        let mut rows = Vec::new();
        for snapshot in self.files.iter().take(host::FILE_ROWS) {
            let key = format!("home/file/{}", snapshot.short_id);
            let message = match snapshot.message.is_empty() {
                true => "(no message)".to_owned(),
                false => snapshot.message.clone(),
            };
            rows.push(kit::sized(
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            kit::mono(format!("{key}/id"), &snapshot.short_id),
                            kit::sized(
                                kit::nowrap(kit::text(format!("{key}/message"), message)),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::secondary(format!("{key}/author"), &snapshot.author),
                            kit::mono(
                                format!("{key}/height"),
                                host::height_label(snapshot.height),
                            ),
                        ],
                    ),
                    8.,
                ),
                None,
                Some(Length::Fixed(LIST_ROW)),
            ));
        }
        if rows.is_empty() {
            rows.push(kit::empty_state(
                "home/files/empty",
                "No snapshots",
                "Nothing has been committed to the shared files yet.",
            ));
        }
        self.card(
            "files",
            "Recent files",
            kit::spaced(kit::column("home/files/list", rows), 0.),
        )
    }

    /// A pressable row with the label a reader hears (what the app's tests
    /// press).
    fn labelled(mut row: Node, accessible: String) -> Node {
        let Node::Button { label, .. } = &mut row else {
            unreachable!("a list row is a button")
        };
        *label = Some(accessible);
        row
    }
}
