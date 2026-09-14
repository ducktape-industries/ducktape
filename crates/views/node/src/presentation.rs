use crate::{Message, NodeTab, NodeView, host};
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{self, AlignX, ButtonPreset, Length, Node},
};

/// Every tab: the state it selects, its key, its name and what a reader
/// hears. The accessible label is what the app's own tests press.
const TABS: [(NodeTab, &str, &str, &str); 4] = [
    (NodeTab::Overview, "overview", "Overview", "Node overview"),
    (
        NodeTab::Permissions,
        "permissions",
        "Permissions",
        "Node permissions",
    ),
    (NodeTab::Activity, "activity", "Activity", "Node activity"),
    (NodeTab::Modules, "modules", "Modules", "Node modules"),
];

/// The console's level chips: the chip key, its name, and the level it keeps
/// (empty for every level).
const LEVELS: [(&str, &str, &str); 6] = [
    ("all", "All", ""),
    ("error", "Error", "ERROR"),
    ("warn", "Warn", "WARN"),
    ("info", "Info", "INFO"),
    ("debug", "Debug", "DEBUG"),
    ("trace", "Trace", "TRACE"),
];

/// The densities this screen is built on: a reading, a list row, a module
/// row and a console line.
const READING_ROW: f32 = 24.;
const LIST_ROW: f32 = 28.;
const MODULE_ROW: f32 = 32.;

impl NodeView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let live = self.connected;
        let mut head = vec![kit::sized(
            kit::title("node/title", "This node"),
            Some(Length::Fill),
            None,
        )];
        // the app's connection reading is a badge only once there is one:
        // an empty pill says nothing
        if !self.status.is_empty() {
            head.push(kit::badge(
                "node/connection",
                &self.status,
                if live { Tone::Success } else { Tone::Neutral },
            ));
        }
        let mut body = vec![kit::centered_row("node/head", head), self.tab_row()];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "node/error",
                kit::wrapping(kit::text(
                    "node/error-text",
                    format!("Could not read this node: {}", self.host_error),
                )),
                Tone::Danger,
            ));
        }
        let panel = match (self.connected, self.node_tab) {
            (false, _) => kit::empty_state(
                "node/disconnected",
                "Not connected",
                "Choose a network from the sidebar to read this node.",
            ),
            (true, NodeTab::Overview) => self.overview(),
            (true, NodeTab::Permissions) => self.permissions(),
            (true, NodeTab::Activity) => self.activity(),
            (true, NodeTab::Modules) => self.modules(),
        };
        body.push(panel);
        kit::page("node/content", body)
    }

    /// The tab row over its hairline: the kit's own switches, carrying the
    /// labels a reader hears.
    fn tab_row(&self) -> Node {
        let chosen = self.node_tab;
        let mut tabs = Self::switches(
            "node/tab",
            TABS.map(|(tab, id, name, _)| {
                (
                    id.to_owned(),
                    name.to_owned(),
                    tab == chosen,
                    Some(slots::message(Message::SelectNodeTab(tab))),
                )
            }),
        );
        let Node::Linear { children, .. } = &mut tabs else {
            unreachable!("a tab row is a row")
        };
        for (button, (.., accessible)) in children.iter_mut().zip(TABS) {
            let Node::Button { label, .. } = button else {
                unreachable!("a tab is a button")
            };
            *label = Some(accessible.to_owned());
        }
        kit::spaced(
            kit::column("node/tabs", [tabs, kit::divider("node/tabs/rule")]),
            6.,
        )
    }

    fn overview(&self) -> Node {
        let facts = &self.facts;
        let chain = kit::card(
            "node/chain",
            Self::readings(
                "node/chain-readings",
                [
                    Self::reading(
                        "node/height",
                        "Height",
                        kit::mono(
                            "node/height/value",
                            host::height_label_short(facts.node_height),
                        ),
                    ),
                    Self::reading(
                        "node/checkpoint",
                        "Checkpoint",
                        kit::mono(
                            "node/checkpoint/value",
                            host::height_label_short(facts.node_checkpoint),
                        ),
                    ),
                    Self::reading(
                        "node/finalized",
                        "Last finalized",
                        kit::text(
                            "node/finalized/value",
                            host::relative_time(facts.node_last_finalized, self.wall_now),
                        ),
                    ),
                    Self::reading(
                        "node/quorum",
                        "Reachable / quorum",
                        kit::mono(
                            "node/quorum/value",
                            host::reading_pair(
                                &facts.node_reachable_label,
                                &facts.node_quorum_label,
                            ),
                        ),
                    ),
                    Self::reading(
                        "node/sync",
                        "Synchronization",
                        kit::text("node/sync/value", &facts.sync_line),
                    ),
                    Self::reading(
                        "node/phase-since",
                        "Phase since",
                        kit::text(
                            "node/phase-since/value",
                            host::relative_time(facts.node_phase_since, self.wall_now),
                        ),
                    ),
                    Self::reading(
                        "node/retries",
                        "Sync retries",
                        kit::mono("node/retries/value", facts.node_sync_retries.to_string()),
                    ),
                    Self::reading(
                        "node/failures",
                        "Sync failures",
                        kit::mono("node/failures/value", facts.node_sync_failures.to_string()),
                    ),
                ],
            ),
        );
        let identity_rows = [
            Self::copyable("node/key", "Node key", &facts.node_key),
            Self::copyable("node/root", "Root hash", &facts.node_root_hash),
            Self::copyable("node/directory", "Data directory", &self.node_data_dir),
            Self::reading(
                "node/version",
                "Version",
                kit::mono("node/version/value", &facts.node_version),
            ),
            kit::gap(4.),
            kit::row(
                "node/identity-actions",
                [kit::sized(
                    kit::button(
                        "node/open-modules",
                        "Installed modules",
                        Some(slots::message(Message::OpenNodeModules)),
                        ButtonPreset::Secondary,
                    ),
                    None,
                    Some(Length::Fixed(LIST_ROW)),
                )],
            ),
        ];
        let identity = kit::card(
            "node/identity",
            Self::readings("node/identity-readings", identity_rows),
        );
        let mut peers = Vec::new();
        for peer in &self.node_peers {
            let key = format!("node/peer/{}", peer.key);
            let (status, tone) = if peer.live {
                ("Connected", Tone::Success)
            } else {
                ("Disconnected", Tone::Neutral)
            };
            if !peers.is_empty() {
                peers.push(kit::divider(format!("{key}/rule")));
            }
            // the row shows the head of the key; the copy takes all of it
            let mut copy = kit::button(
                format!("{key}/copy"),
                "Copy",
                Some(slots::message(Message::CopyToClipboard(
                    peer.key.clone(),
                    "Peer key copied".into(),
                ))),
                ButtonPreset::Subtle,
            );
            let Node::Button { label, .. } = &mut copy else {
                unreachable!("a copy control is a button")
            };
            *label = Some(format!("Copy peer key {}", host::short_label(&peer.key)));
            peers.push(Self::list_row(
                &key,
                [
                    kit::sized(
                        kit::nowrap(kit::mono(
                            format!("{key}/key"),
                            host::short_label(&peer.key),
                        )),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::badge(
                        format!("{key}/role"),
                        host::capitalized(&peer.role),
                        Tone::Neutral,
                    ),
                    kit::badge(format!("{key}/status"), status, tone),
                    copy,
                ],
            ));
        }
        if self.node_peers.is_empty() {
            peers.push(kit::empty_state(
                "node/peers/empty",
                "No direct peers",
                "This node is not connected to any other node right now.",
            ));
        }
        let mut sections = vec![Self::section(
            "node/chain-section",
            "node/chain-title",
            "Chain",
            chain,
        )];
        if !facts.node_sync_last_error.is_empty() {
            sections.push(kit::notice(
                "node/sync-error",
                kit::wrapping(kit::text(
                    "node/sync-error/text",
                    format!(
                        "The last sync attempt failed: {}",
                        facts.node_sync_last_error
                    ),
                )),
                Tone::Warning,
            ));
        }
        sections.push(Self::section(
            "node/identity-section",
            "node/identity-title",
            "Identity",
            identity,
        ));
        sections.push(Self::section(
            "node/peers",
            "node/peers/title",
            "Peers",
            kit::spaced(kit::column("node/peers/list", peers), 0.),
        ));
        kit::scroll(
            "node/overview",
            kit::spaced(kit::column("node/readings", sections), 16.),
        )
    }

    fn permissions(&self) -> Node {
        let description = match self.tier.as_str() {
            "validator" => "Signs quorum, finalizes rounds and stores full history.",
            "resident" => "Stores full history without signing quorum.",
            "guest" => "Reads and verifies finalized headers.",
            _ => "Node standing has not been reported.",
        };
        let admin = if self.admin {
            "Available to this seat"
        } else {
            "Not available to this seat"
        };
        let tier = match self.tier.is_empty() {
            true => "Not reported".to_owned(),
            false => host::capitalized(&self.tier),
        };
        let capability = |key: &str, name: &str, tiers: [bool; 3]| {
            Self::list_row(
                key,
                [
                    kit::sized(
                        kit::text(format!("{key}/label"), name),
                        Some(Length::Fill),
                        None,
                    ),
                    Self::mark(format!("{key}/validator"), tiers[0]),
                    Self::mark(format!("{key}/resident"), tiers[1]),
                    Self::mark(format!("{key}/guest"), tiers[2]),
                ],
            )
        };
        let standing = kit::card(
            "node/standing-card",
            Self::readings(
                "node/standing",
                [
                    Self::reading(
                        "node/tier",
                        "Standing",
                        kit::badge("node/tier/value", tier, Tone::Accent),
                    ),
                    Self::reading(
                        "node/admin",
                        "Node administration",
                        kit::text("node/admin/value", admin),
                    ),
                    kit::gap(4.),
                    kit::wrapping(kit::secondary("node/standing-description", description)),
                    kit::wrapping(kit::caption(
                        "node/quorum-note",
                        "Quorum standing is granted and revoked by quorum, not by this device.",
                    )),
                ],
            ),
        );
        let matrix = kit::spaced(
            kit::column(
                "node/permissions/matrix",
                [
                    Self::list_row(
                        "node/permissions/header",
                        [
                            kit::sized(
                                kit::caption("node/permissions/capability", "Capability"),
                                Some(Length::Fill),
                                None,
                            ),
                            Self::cell(
                                "node/permissions/header/validator",
                                kit::caption("node/permissions/validator", "Validator"),
                            ),
                            Self::cell(
                                "node/permissions/header/resident",
                                kit::caption("node/permissions/resident", "Resident"),
                            ),
                            Self::cell(
                                "node/permissions/header/guest",
                                kit::caption("node/permissions/guest", "Guest"),
                            ),
                        ],
                    ),
                    kit::divider("node/permissions/rule"),
                    capability(
                        "node/permissions/read",
                        "Read and verify finality",
                        [true, true, true],
                    ),
                    kit::divider("node/permissions/rule/propose"),
                    capability(
                        "node/permissions/propose",
                        "Propose modules and members",
                        [true, true, false],
                    ),
                    kit::divider("node/permissions/rule/sign"),
                    capability(
                        "node/permissions/sign",
                        "Sign quorum and finalize",
                        [true, false, false],
                    ),
                ],
            ),
            0.,
        );
        kit::scroll(
            "node/permissions",
            kit::spaced(
                kit::column(
                    "node/permissions/sections",
                    [
                        Self::section(
                            "node/standing-section",
                            "node/standing-title",
                            "This seat",
                            standing,
                        ),
                        Self::section(
                            "node/matrix-section",
                            "node/matrix-title",
                            "What each standing may do",
                            matrix,
                        ),
                    ],
                ),
                16.,
            ),
        )
    }

    fn modules(&self) -> Node {
        let mut content = Vec::new();
        for module in &self.module_rows {
            let key = format!("node/module/{}", module.id);
            let (state, tone) = match (module.pending_hash.is_empty(), module.ready) {
                (true, _) => ("Active", Tone::Neutral),
                (false, true) => ("Swap ready", Tone::Success),
                (false, false) => ("Swap pending", Tone::Warning),
            };
            if !content.is_empty() {
                content.push(kit::divider(format!("{key}/rule")));
            }
            content.push(kit::sized(
                kit::centered_row(
                    &key,
                    [
                        kit::nowrap(kit::strong(format!("{key}/name"), &module.id)),
                        kit::spacer(),
                        kit::badge(
                            format!("{key}/category"),
                            host::capitalized(&module.category),
                            Tone::Neutral,
                        ),
                        kit::badge(format!("{key}/state"), state, tone),
                    ],
                ),
                Some(Length::Fill),
                Some(Length::Fixed(MODULE_ROW)),
            ));
            // the hashes are what an operator compares across nodes: the row
            // shows the head of each, the copy takes all of it
            content.push(Self::digest(
                &format!("{key}/root"),
                "State root",
                &module.root,
            ));
            content.push(Self::digest(
                &format!("{key}/code"),
                "Active code",
                &module.code_hash,
            ));
            if module.pending_hash.is_empty() {
                continue;
            }
            content.push(Self::reading(
                &format!("{key}/pending"),
                "Pending code",
                kit::centered_row(
                    format!("{key}/pending/row"),
                    [
                        kit::nowrap(kit::mono(
                            format!("{key}/pending/value"),
                            host::short_digest(&module.pending_hash),
                        )),
                        kit::caption(
                            format!("{key}/activation"),
                            format!(
                                "activates at {}",
                                host::height_label_short(module.activation_height)
                            ),
                        ),
                        kit::caption(
                            format!("{key}/readiness"),
                            format!("{} signalled ready", module.readiness),
                        ),
                        kit::button(
                            format!("{key}/pending/copy"),
                            "Copy pending code",
                            Some(slots::message(Message::CopyToClipboard(
                                module.pending_hash.clone(),
                                "Pending code copied".into(),
                            ))),
                            ButtonPreset::Subtle,
                        ),
                    ],
                ),
            ));
        }
        if content.is_empty() {
            content.push(kit::empty_state(
                "node/modules/empty",
                "No modules reported",
                "The node lists its modules once it has read its genesis.",
            ));
        }
        kit::scroll(
            "node/modules",
            kit::spaced(kit::column("node/module-list", content), 0.),
        )
    }

    fn activity(&self) -> Node {
        let filter = kit::input(
            "node/log-filter",
            "Filter lines",
            &self.node_log_filter,
            slots::handler(Box::new(|value: String| {
                Some(Message::NodeLogFilterChanged(value))
            })),
            None,
        );
        let live = kit::input(
            "node/live-filter",
            "info,ducktape::join=debug",
            &self.live_log_filter,
            slots::handler(Box::new(|value: String| {
                Some(Message::LiveLogFilterChanged(value))
            })),
            Some(slots::message(Message::ApplyLiveLogFilter)),
        );
        let can_retune = self.admin && !self.live_log_filter.trim().is_empty();
        // what the retune row says beside its control: why it is closed to
        // this seat, or how the last retune went
        let noted = !self.live_filter_note.is_empty();
        let retune_note = match (self.admin, noted, self.live_filter_failed) {
            (false, ..) => Some(kit::caption(
                "node/retune/closed",
                "Only this node's operator can retune its tracing filter.",
            )),
            (true, false, _) => None,
            (true, true, true) => Some(kit::tone_text(
                "node/filter-note",
                &self.live_filter_note,
                Tone::Danger,
            )),
            (true, true, false) => Some(kit::caption("node/filter-note", &self.live_filter_note)),
        };
        // the console's own filters on one line, the node's tracing filter
        // on the next: two controls with different reach do not share a row
        let mut retune = vec![
            kit::nowrap(kit::secondary("node/retune/label", "Tracing filter")),
            kit::sized(live, Some(Length::Fixed(240.)), None),
            kit::sized(
                kit::button(
                    "node/retune/apply",
                    "Retune",
                    can_retune.then(|| slots::message(Message::ApplyLiveLogFilter)),
                    ButtonPreset::Secondary,
                ),
                None,
                Some(Length::Fixed(LIST_ROW)),
            ),
        ];
        retune.extend(retune_note.map(kit::wrapping));
        let mut content = vec![
            kit::centered_row("node/filters", [self.level_chips(), filter]),
            kit::centered_row("node/retune", retune),
        ];
        let visible =
            host::visible_log(&self.log_lines, &self.node_log_filter, &self.node_log_level);
        let note = host::log_note(self.log_lines.len() as i64, visible.len() as i64);
        if !note.is_empty() {
            content.push(kit::caption("node/log-note", note));
        }
        let p = kit::palette();
        let rows = visible.iter().map(|line| {
            let key = format!("node/log/{}", line.cursor);
            let level_color = match line.level.as_str() {
                "ERROR" => p.danger,
                "WARN" => p.warning,
                "DEBUG" | "TRACE" => p.faint,
                _ => p.muted,
            };
            kit::spaced(
                kit::row(
                    &key,
                    [
                        kit::nowrap(kit::colored(
                            kit::mono(format!("{key}/time"), &line.time),
                            p.faint,
                        )),
                        kit::sized(
                            kit::nowrap(kit::colored(
                                kit::weighted(
                                    kit::mono(format!("{key}/level"), &line.level),
                                    wire::Weight::Medium,
                                ),
                                level_color,
                            )),
                            Some(Length::Fixed(52.)),
                            None,
                        ),
                        kit::sized(
                            kit::wrapping(kit::mono(format!("{key}/message"), &line.message)),
                            Some(Length::Fill),
                            None,
                        ),
                    ],
                ),
                8.,
            )
        });
        let mut log = kit::scroll(
            "node/logs",
            kit::padded(
                kit::spaced(kit::column("node/log-lines", rows), 2.),
                wire::Edges::all(8.),
            ),
        );
        if let Node::Scroll {
            background,
            border,
            anchor_y,
            auto_scroll,
            ..
        } = &mut log
        {
            // a console opens at its tail and follows it while the reader is
            // there; scrolling up holds the place
            *anchor_y = wire::ScrollAnchor::End;
            *auto_scroll = true;
            *background = Some(kit::rgba(p.surface));
            *border = Some(wire::Border {
                color: Some(kit::rgba(p.border)),
                width: Some(1.),
                radius: Some([kit::radius::CARD as f32; 4]),
            });
        }
        content.push(log);
        kit::sized(
            kit::spaced(kit::column("node/activity", content), 8.),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }

    /// A row of switches at the list-row height.
    fn switches(
        key: &str,
        choices: impl IntoIterator<Item = (String, String, bool, Option<u32>)>,
    ) -> Node {
        let mut node = kit::tabs(key, choices);
        let Node::Linear { children, .. } = &mut node else {
            unreachable!("a switch row is a row")
        };
        for button in children.iter_mut() {
            let Node::Button { height, .. } = button else {
                unreachable!("a switch is a button")
            };
            *height = Some(Length::Fixed(LIST_ROW));
        }
        node
    }

    /// The console's level chips, the chosen one checked.
    fn level_chips(&self) -> Node {
        let chosen = self.node_log_level.as_str();
        let chips = Self::switches(
            "node/level",
            LEVELS.map(|(id, name, level)| {
                (
                    id.to_owned(),
                    name.to_owned(),
                    level == chosen,
                    Some(slots::message(Message::SelectLogLevel(level.to_owned()))),
                )
            }),
        );
        kit::sized(chips, Some(Length::Shrink), None)
    }

    fn section(key: &str, title_key: &str, title: &str, content: Node) -> Node {
        kit::spaced(
            kit::column(key, [kit::heading(title_key, title), content]),
            8.,
        )
    }

    /// The rows of a reading card: tight, because each row owns its height.
    fn readings(key: &str, rows: impl IntoIterator<Item = Node>) -> Node {
        kit::spaced(kit::column(key, rows), 2.)
    }

    /// One reading: a fixed-width label and its value, on one 24px line.
    fn reading(key: &str, label: &str, value: Node) -> Node {
        kit::sized(
            kit::aligned(kit::kv(key, label, value), AlignX::Center),
            Some(Length::Fill),
            Some(Length::Fixed(READING_ROW)),
        )
    }

    /// One row of a list or a table: 28px, its children on one centre line.
    fn list_row(key: &str, children: impl IntoIterator<Item = Node>) -> Node {
        kit::sized(
            kit::centered_row(key, children),
            Some(Length::Fill),
            Some(Length::Fixed(LIST_ROW)),
        )
    }

    /// One column of the permissions table: a fixed width, centred.
    fn cell(key: impl Into<String>, child: Node) -> Node {
        let mut node = kit::container(key, child);
        let Node::Container { width, align_x, .. } = &mut node else {
            unreachable!("a cell is a container")
        };
        *width = Some(Length::Fixed(72.));
        *align_x = Some(AlignX::Center);
        node
    }

    /// Whether a standing holds a capability.
    fn mark(key: String, allowed: bool) -> Node {
        let text = match allowed {
            true => kit::tone_text(format!("{key}/mark"), "✓", Tone::Success),
            false => kit::colored(
                kit::text_size(
                    kit::text(format!("{key}/mark"), "–"),
                    kit::type_scale::SECONDARY as f32,
                ),
                kit::palette().faint,
            ),
        };
        Self::cell(key, text)
    }

    /// A digest reading: the head of it on the row, all of it on the copy.
    fn digest(key: &str, label: &str, value: &str) -> Node {
        Self::copyable_showing(key, label, value, &host::short_digest(value))
    }

    /// A reading the reader can take with them: the value, and a ghost copy
    /// control at the right of its row.
    fn copyable(key: &str, label: &str, value: &str) -> Node {
        Self::copyable_showing(key, label, value, value)
    }

    /// A value the node has not reported is a reading, not a blank with a
    /// copy control that copies nothing.
    fn copyable_showing(key: &str, label: &str, value: &str, shown: &str) -> Node {
        if value.is_empty() {
            return Self::reading(
                key,
                label,
                kit::colored(
                    kit::text(format!("{key}/value"), "Not reported"),
                    kit::palette().faint,
                ),
            );
        }
        let mut copy = kit::button(
            format!("{key}/copy"),
            "Copy",
            Some(slots::message(Message::CopyToClipboard(
                value.into(),
                format!("{label} copied"),
            ))),
            ButtonPreset::Subtle,
        );
        let Node::Button {
            label: accessible,
            height,
            ..
        } = &mut copy
        else {
            unreachable!("a copy control is a button")
        };
        *accessible = Some(format!("Copy {}", label.to_lowercase()));
        *height = Some(Length::Fixed(READING_ROW));
        kit::aligned(
            kit::kv(
                key,
                label,
                kit::centered_row(
                    format!("{key}/reading"),
                    [
                        kit::sized(
                            kit::wrapping(kit::mono(format!("{key}/value"), shown)),
                            Some(Length::Fill),
                            None,
                        ),
                        copy,
                    ],
                ),
            ),
            AlignX::Center,
        )
    }
}
