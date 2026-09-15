//! The dashboard's tree: a head, a strip of stat tiles, then the cards
//! dealt into as many columns as the pane affords — each card a reading of
//! one plane and, where the `duck://` grammar names one, a door to its
//! tab. Same ink as the sibling views: a title row, cards over a hairline,
//! mono for keys and heights, a badge for a state.
//!
//! A sensor around the scrolling body measures the pane; the column count
//! and the tiles per line follow that width, never the window's.

use crate::{HomeView, Message, host};
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{ButtonPreset, Length, Node},
};

/// The density a card's row is built on.
const LIST_ROW: f32 = 28.;

/// The gutter between tiles, between columns and between the cards of one
/// column.
const GUTTER: f32 = 16.;

/// The type size a stat tile's number is set at: the scale's title, the
/// loudest a view gets.
const TILE_VALUE_SIZE: f32 = kit::type_scale::TITLE as f32;

// Every cell of a row is ONE LINE. The rows are built at a fixed height
// (`LIST_ROW`) and a card in a three-column pane is narrow: a height, a
// count or a digest allowed to wrap breaks onto a second line under the
// next row. The one text that may wrap — a notice — says so with
// `kit::wrapping`, which overrides this.

fn text(key: impl Into<String>, content: impl Into<String>) -> Node {
    kit::nowrap(kit::text(key, content))
}

fn mono(key: impl Into<String>, content: impl Into<String>) -> Node {
    kit::nowrap(kit::mono(key, content))
}

fn secondary(key: impl Into<String>, content: impl Into<String>) -> Node {
    kit::nowrap(kit::secondary(key, content))
}

impl HomeView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let mut head = vec![kit::sized(
            kit::title("home/title", "Home"),
            Some(Length::Fill),
            None,
        )];
        if self.connected && !self.facts.version.is_empty() {
            head.push(kit::caption(
                "home/version",
                format!("ducktape {}", self.facts.version),
            ));
        }
        if self.connected && !self.facts.phase.is_empty() {
            head.push(kit::badge("home/phase", &self.facts.phase, Tone::Success));
        }
        let mut body = vec![kit::centered_row("home/head", head)];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "home/error",
                kit::wrapping(text(
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
        let columns = host::columns_for(self.viewport_width);
        let cards = vec![
            self.node_card(),
            self.members_card(),
            self.peers_card(),
            self.blocks_card(),
            self.rooms_card(),
            self.runs_card(),
            self.proposals_card(),
            self.files_card(),
            self.modules_card(),
        ];
        let rails = host::dealt(cards, columns)
            .into_iter()
            .enumerate()
            .map(|(index, cards)| {
                kit::sized(
                    kit::spaced(kit::column(format!("home/column/{index}"), cards), GUTTER),
                    Some(Length::FillPortion(1)),
                    None,
                )
            });
        let dashboard = kit::spaced(
            kit::column(
                "home/dashboard",
                [
                    self.stat_strip(host::tiles_per_row(columns)),
                    kit::spaced(kit::row("home/columns", rails), GUTTER),
                ],
            ),
            GUTTER,
        );
        let observe = || {
            slots::handler::<(f32, f32), Message>(Box::new(|(width, height)| {
                Some(Message::ViewportChanged(width.into(), height.into()))
            }))
        };
        body.push(Node::Sensor {
            key: "home/viewport".into(),
            reset: None,
            on_show: Some(observe()),
            on_resize: Some(observe()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(kit::scroll("home/cards", dashboard)),
        });
        kit::page("home/content", body)
    }

    // ---------- the stat strip ----------

    /// The numbers a reader wants before any card: one tile each, in lines
    /// of `per_row`, the last line padded so every tile keeps one width.
    fn stat_strip(&self, per_row: usize) -> Node {
        let peers_online = match self.peers.is_empty() {
            true => "—".to_owned(),
            false => format!(
                "{} / {}",
                host::grouped_digits(host::live_peers(&self.peers)),
                host::grouped_digits(host::count_i64(self.peers.len()))
            ),
        };
        let active_runs = self
            .runs
            .iter()
            .filter(|run| matches!(run.state.as_str(), "running" | "dispatched"))
            .count();
        let tiles = [
            (
                "height",
                "Block height",
                host::count_label(self.facts.height),
            ),
            (
                "validators",
                "Validators",
                host::grouped_digits(self.roster.validators),
            ),
            (
                "residents",
                "Residents",
                host::grouped_digits(self.roster.residents),
            ),
            ("peers", "Peers online", peers_online),
            (
                "rooms",
                "Rooms",
                host::grouped_digits(host::count_i64(self.rooms.len())),
            ),
            (
                "proposals",
                "Open proposals",
                host::grouped_digits(host::count_i64(self.proposals.len())),
            ),
            (
                "runs",
                "Active runs",
                host::grouped_digits(host::count_i64(active_runs)),
            ),
            (
                "modules",
                "Modules",
                host::grouped_digits(host::count_i64(self.facts.modules.len())),
            ),
        ];
        let mut lines = Vec::new();
        for (line, chunk) in tiles.chunks(per_row.max(1)).enumerate() {
            let mut cells: Vec<Node> = chunk
                .iter()
                .map(|(name, label, value)| Self::tile(name, label, value))
                .collect();
            while cells.len() < per_row {
                cells.push(kit::space(Some(Length::FillPortion(1)), None));
            }
            lines.push(kit::spaced(
                kit::row(format!("home/stats/{line}"), cells),
                GUTTER,
            ));
        }
        kit::spaced(kit::column("home/stats", lines), GUTTER)
    }

    fn tile(name: &str, label: &str, value: &str) -> Node {
        let key = format!("home/stat/{name}");
        let body = kit::spaced(
            kit::column(
                format!("{key}/body"),
                [
                    kit::text_size(mono(format!("{key}/value"), value), TILE_VALUE_SIZE),
                    kit::caption(format!("{key}/label"), label),
                ],
            ),
            4.,
        );
        kit::sized(kit::card(key, body), Some(Length::FillPortion(1)), None)
    }

    // ---------- the cards ----------

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
                kit::wrapping(secondary(
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
        kit::sized(
            kit::kv(key, name, value),
            None,
            Some(Length::Fixed(LIST_ROW)),
        )
    }

    /// A reading of a digest: its head in mono, and a button that copies
    /// the whole of it.
    fn copyable(key: &str, name: &str, digest: &str, copied: &str) -> Node {
        kit::centered_row(
            key.to_owned(),
            [
                kit::sized(
                    kit::kv(
                        format!("{key}/reading"),
                        name,
                        mono(format!("{key}/value"), host::short_label(digest)),
                    ),
                    Some(Length::Fill),
                    None,
                ),
                kit::button(
                    format!("{key}/copy"),
                    "Copy",
                    Some(slots::message(Message::CopyToClipboard(
                        digest.to_owned(),
                        copied.to_owned(),
                    ))),
                    ButtonPreset::Subtle,
                ),
            ],
        )
    }

    fn node_card(&self) -> Node {
        let facts = &self.facts;
        let mut readings = vec![
            Self::reading(
                "home/node/height",
                "Height",
                mono("home/node/height/value", host::height_label(facts.height)),
            ),
            Self::reading(
                "home/node/sync",
                "Doing",
                text("home/node/sync/value", &facts.sync_line),
            ),
            Self::reading(
                "home/node/chain",
                "Network",
                mono("home/node/chain/value", self.chain_id()),
            ),
        ];
        // consensus facts are a validator's alone: the projection leaves the
        // section out on a resident rather than filling it with zeros
        let signs = facts.quorum >= 0 && facts.reachable_validators >= 0;
        if signs {
            readings.push(Self::reading(
                "home/node/quorum",
                "Reachable",
                mono(
                    "home/node/quorum/value",
                    format!(
                        "{} of {} for quorum",
                        host::grouped_digits(facts.reachable_validators),
                        host::grouped_digits(facts.quorum)
                    ),
                ),
            ));
        }
        readings.push(Self::reading(
            "home/node/checkpoint",
            "Checkpoint",
            mono(
                "home/node/checkpoint/value",
                host::height_label(facts.checkpoint_height),
            ),
        ));
        readings.push(Self::copyable(
            "home/node/root",
            "Root hash",
            &facts.root_hash,
            "Root hash copied",
        ));
        readings.push(Self::copyable(
            "home/node/key",
            "Node key",
            &facts.node_key,
            "Node key copied",
        ));
        if !facts.sync_last_error.is_empty() {
            readings.push(kit::notice(
                "home/node/sync-error",
                kit::wrapping(secondary(
                    "home/node/sync-error/text",
                    format!(
                        "Sync failed {} times; last: {}",
                        host::grouped_digits(facts.sync_failures),
                        facts.sync_last_error
                    ),
                )),
                Tone::Warning,
            ));
        }
        self.card(
            "node",
            "This node",
            kit::column("home/node/readings", readings),
        )
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
                    mono(
                        "home/members/validators/value",
                        host::grouped_digits(roster.validators),
                    ),
                ),
                Self::reading(
                    "home/members/residents",
                    "Residents",
                    mono(
                        "home/members/residents/value",
                        host::grouped_digits(roster.residents),
                    ),
                ),
                Self::reading(
                    "home/members/tier",
                    "This node",
                    kit::badge("home/members/tier/value", tier, Tone::Neutral),
                ),
                Self::reading(
                    "home/members/account",
                    "Signed in as",
                    mono(
                        "home/members/account/value",
                        match self.account.is_empty() {
                            true => "—".to_owned(),
                            false => format!("acct:{}", self.account),
                        },
                    ),
                ),
            ],
        );
        self.card("members", "Members", readings)
    }

    fn peers_card(&self) -> Node {
        let mut rows = Vec::new();
        for peer in self.peers.iter().take(host::PEER_ROWS) {
            let key = format!("home/peer/{}", peer.key);
            let (state, tone) = match peer.live {
                true => ("Online", Tone::Success),
                false => ("Offline", Tone::Neutral),
            };
            rows.push(kit::sized(
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            kit::sized(
                                mono(format!("{key}/key"), host::short_label(&peer.key)),
                                Some(Length::Fill),
                                None,
                            ),
                            secondary(format!("{key}/role"), host::capitalized(&peer.role)),
                            kit::badge(format!("{key}/state"), state, tone),
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
                "home/peers/empty",
                "No peers",
                "This node has not met another node yet.",
            ));
        }
        self.card(
            "peers",
            "Peers",
            kit::spaced(kit::column("home/peers/list", rows), 0.),
        )
    }

    fn blocks_card(&self) -> Node {
        let mut rows = Vec::new();
        for block in self.blocks.iter().take(host::BLOCK_ROWS) {
            let key = format!("home/block/{}", block.height);
            rows.push(kit::sized(
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            mono(format!("{key}/height"), host::height_label(block.height)),
                            kit::sized(
                                mono(format!("{key}/hash"), host::short_label(&block.hash)),
                                Some(Length::Fill),
                                None,
                            ),
                            secondary(
                                format!("{key}/ops"),
                                format!("{} ops", host::grouped_digits(block.op_count)),
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
                "home/blocks/empty",
                "No blocks with operations",
                "Nothing has been written to this network yet.",
            ));
        }
        self.card(
            "blocks",
            "Recent blocks",
            kit::spaced(kit::column("home/blocks/list", rows), 0.),
        )
    }

    fn modules_card(&self) -> Node {
        let mut rows = Vec::new();
        for module in self.facts.modules.iter().take(host::MODULE_ROWS) {
            let key = format!("home/module/{}", module.id);
            rows.push(kit::sized(
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/row"),
                        [
                            kit::sized(
                                text(format!("{key}/id"), &module.id),
                                Some(Length::Fill),
                                None,
                            ),
                            secondary(
                                format!("{key}/category"),
                                host::capitalized(&module.category),
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
                "home/modules/empty",
                "No modules",
                "The node reported no module set.",
            ));
        }
        self.card(
            "modules",
            "Modules",
            kit::spaced(kit::column("home/modules/list", rows), 0.),
        )
    }

    fn rooms_card(&self) -> Node {
        let rooms = host::rooms_with_news(&self.rooms, &self.rooms_seen);
        let mut rows = Vec::new();
        for (room, moved) in rooms.iter().take(host::ROOM_ROWS) {
            let key = format!("home/room/{}", room.id);
            let mut cells = vec![kit::sized(
                text(format!("{key}/name"), format!("#{}", room.name)),
                Some(Length::Fill),
                None,
            )];
            if *moved {
                cells.push(kit::badge(format!("{key}/new"), "New", Tone::Success));
            }
            cells.push(mono(
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
                                text(format!("{key}/agent"), &run.agent_id),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::badge(format!("{key}/state"), host::capitalized(&run.state), tone),
                            mono(
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
                                text(format!("{key}/action"), &proposal.action),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::badge(
                                format!("{key}/approvals"),
                                format!("{} approvals", proposal.approvals),
                                Tone::Neutral,
                            ),
                            mono(
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
                            mono(format!("{key}/id"), &snapshot.short_id),
                            kit::sized(
                                text(format!("{key}/message"), message),
                                Some(Length::Fill),
                                None,
                            ),
                            secondary(format!("{key}/author"), &snapshot.author),
                            mono(format!("{key}/height"), host::height_label(snapshot.height)),
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
