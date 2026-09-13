use crate::{Message, NodeTab, NodeView, host};
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{self, ButtonPreset, Length, Node},
};

impl NodeView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let tabs = [
            (NodeTab::Overview, "Overview", "Node overview"),
            (NodeTab::Permissions, "Permissions", "Node permissions"),
            (NodeTab::Activity, "Activity", "Node activity"),
            (NodeTab::Modules, "Modules", "Node modules"),
        ]
        .map(|(tab, title, accessible)| {
            let selected = self.node_tab == tab;
            let mut button = kit::button(
                format!("node/tab/{title}"),
                title,
                Some(slots::message(Message::SelectNodeTab(tab))),
                ButtonPreset::Subtle,
            );
            if let Node::Button { label, checked, .. } = &mut button {
                *label = Some(accessible.into());
                *checked = Some(selected);
            }
            button
        });
        let live = self.connected;
        let mut body = vec![
            kit::centered_row(
                "node/head",
                [
                    kit::sized(
                        kit::title("node/title", "This node"),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::badge(
                        "node/connection",
                        &self.status,
                        if live { Tone::Success } else { Tone::Neutral },
                    ),
                ],
            ),
            kit::spaced(kit::row("node/tabs", tabs), 2.),
        ];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "node/error",
                kit::wrapping(kit::text("node/error-text", &self.host_error)),
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

    fn overview(&self) -> Node {
        let facts = &self.facts;
        let chain = kit::card(
            "node/chain",
            kit::spaced(
                kit::column(
                    "node/chain-readings",
                    [
                        Self::reading(
                            "node/height",
                            "Height",
                            kit::mono("node/height/value", host::height_label_short(facts.node_height)),
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
                                host::reading_pair(&facts.node_reachable_label, &facts.node_quorum_label),
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
                10.,
            ),
        );
        let mut identity_rows = vec![
            Self::copyable("node/key", "Node key", &facts.node_key),
            Self::copyable("node/root", "Root hash", &facts.node_root_hash),
            Self::copyable("node/directory", "Data directory", &self.node_data_dir),
            Self::reading(
                "node/version",
                "Version",
                kit::mono("node/version/value", &facts.node_version),
            ),
        ];
        identity_rows.push(kit::row(
            "node/identity-actions",
            [kit::button(
                "node/open-modules",
                "Installed modules",
                Some(slots::message(Message::OpenNodeModules)),
                ButtonPreset::Secondary,
            )],
        ));
        let identity = kit::card(
            "node/identity",
            kit::spaced(kit::column("node/identity-readings", identity_rows), 10.),
        );
        let mut peers = Vec::new();
        for peer in &self.node_peers {
            let key = format!("node/peer/{}", peer.key);
            let (status, tone) = if peer.live {
                ("Connected", Tone::Success)
            } else {
                ("Disconnected", Tone::Neutral)
            };
            peers.push(kit::centered_row(
                &key,
                [
                    kit::sized(
                        kit::wrapping(kit::mono(format!("{key}/key"), &peer.key)),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::badge(format!("{key}/role"), &peer.role, Tone::Neutral),
                    kit::badge(format!("{key}/status"), status, tone),
                ],
            ));
        }
        if self.node_peers.is_empty() {
            peers.push(kit::secondary("node/peers/empty", "No direct peers."));
        }
        let mut sections = vec![
            Self::section("node/chain-section", "node/chain-title", "Chain", chain),
        ];
        if !facts.node_sync_last_error.is_empty() {
            sections.push(kit::notice(
                "node/sync-error",
                kit::wrapping(kit::text(
                    "node/sync-error/text",
                    &facts.node_sync_last_error,
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
            kit::card(
                "node/peers/card",
                kit::spaced(kit::column("node/peers/list", peers), 8.),
            ),
        ));
        kit::scroll(
            "node/overview",
            kit::spaced(kit::column("node/readings", sections), 20.),
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
        let yes = |key: String, value: bool| {
            kit::badge(
                key,
                if value { "Yes" } else { "No" },
                if value { Tone::Success } else { Tone::Neutral },
            )
        };
        let capability = |key: &str, name: &str, tiers: [bool; 3]| {
            kit::centered_row(
                key,
                [
                    kit::sized(
                        kit::text(format!("{key}/label"), name),
                        Some(Length::Fill),
                        None,
                    ),
                    yes(format!("{key}/validator"), tiers[0]),
                    yes(format!("{key}/resident"), tiers[1]),
                    yes(format!("{key}/guest"), tiers[2]),
                ],
            )
        };
        let standing = kit::card(
            "node/standing-card",
            kit::spaced(
                kit::column(
                    "node/standing",
                    [
                        Self::reading(
                            "node/tier",
                            "Standing",
                            kit::badge("node/tier/value", &self.tier, Tone::Accent),
                        ),
                        kit::wrapping(kit::secondary("node/standing-description", description)),
                        Self::reading(
                            "node/admin",
                            "Node administration",
                            kit::text("node/admin/value", admin),
                        ),
                        kit::wrapping(kit::caption(
                            "node/quorum-note",
                            "Quorum standing is granted and revoked by quorum, not by this device.",
                        )),
                    ],
                ),
                10.,
            ),
        );
        let matrix = kit::card(
            "node/permissions/card",
            kit::spaced(
                kit::column(
                    "node/permissions/matrix",
                    [
                        kit::centered_row(
                            "node/permissions/header",
                            [
                                kit::sized(
                                    kit::label("node/permissions/capability", "Capability"),
                                    Some(Length::Fill),
                                    None,
                                ),
                                kit::label("node/permissions/tiers", "Validator · Resident · Guest"),
                            ],
                        ),
                        kit::divider("node/permissions/rule"),
                        capability(
                            "node/permissions/read",
                            "Read and verify finality",
                            [true, true, true],
                        ),
                        capability(
                            "node/permissions/propose",
                            "Propose modules and members",
                            [true, true, false],
                        ),
                        capability(
                            "node/permissions/sign",
                            "Sign quorum and finalize",
                            [true, false, false],
                        ),
                    ],
                ),
                10.,
            ),
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
                20.,
            ),
        )
    }

    fn modules(&self) -> Node {
        let mut content = Vec::new();
        for module in &self.module_rows {
            let key = format!("node/module/{}", module.id);
            let mut values = vec![
                kit::centered_row(
                    format!("{key}/head"),
                    [
                        kit::sized(
                            kit::heading(format!("{key}/title"), &module.id),
                            Some(Length::Fill),
                            None,
                        ),
                        kit::badge(format!("{key}/category"), &module.category, Tone::Neutral),
                    ],
                ),
                Self::copyable(&format!("{key}/root"), "State root", &module.root),
                Self::copyable(&format!("{key}/code"), "Active code", &module.code_hash),
            ];
            if !module.pending_hash.is_empty() {
                values.extend([
                    kit::divider(format!("{key}/pending-rule")),
                    Self::copyable(
                        &format!("{key}/pending"),
                        "Pending code",
                        &module.pending_hash,
                    ),
                    Self::reading(
                        &format!("{key}/activation"),
                        "Activation height",
                        kit::mono(
                            format!("{key}/activation/value"),
                            module.activation_height.to_string(),
                        ),
                    ),
                    Self::reading(
                        &format!("{key}/readiness"),
                        "Readiness",
                        kit::centered_row(
                            format!("{key}/readiness/row"),
                            [
                                kit::mono(
                                    format!("{key}/readiness/value"),
                                    module.readiness.to_string(),
                                ),
                                kit::badge(
                                    format!("{key}/ready"),
                                    if module.ready {
                                        "Ready"
                                    } else {
                                        "Waiting for readiness"
                                    },
                                    if module.ready {
                                        Tone::Success
                                    } else {
                                        Tone::Warning
                                    },
                                ),
                            ],
                        ),
                    ),
                ]);
            }
            content.push(kit::card(
                &key,
                kit::spaced(kit::column(format!("{key}/values"), values), 10.),
            ));
        }
        if content.is_empty() {
            content.push(kit::empty_state(
                "node/modules/empty",
                "No installed modules reported.",
                "The node lists its modules once it has read its genesis.",
            ));
        }
        kit::scroll(
            "node/modules",
            kit::spaced(kit::column("node/module-list", content), 12.),
        )
    }

    fn activity(&self) -> Node {
        let filter = kit::input(
            "node/log-filter",
            "filter logs…",
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
        let mut content = vec![kit::row(
            "node/filters",
            [
                kit::field("node/log-filter-field", "Show lines matching", filter),
                kit::field(
                    "node/retune-field",
                    "Live log filter on the node",
                    kit::row(
                        "node/retune",
                        [
                            live,
                            kit::button(
                                "node/retune/apply",
                                "Retune",
                                can_retune.then(|| slots::message(Message::ApplyLiveLogFilter)),
                                ButtonPreset::Secondary,
                            ),
                        ],
                    ),
                ),
            ],
        )];
        if !self.live_filter_note.is_empty() {
            content.push(kit::caption("node/filter-note", &self.live_filter_note));
        }
        let visible = host::visible_log(&self.log_lines, &self.node_log_filter);
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
                10.,
            )
        });
        let mut log = kit::scroll(
            "node/logs",
            kit::padded(
                kit::spaced(kit::column("node/log-lines", rows), 3.),
                wire::Edges::all(10.),
            ),
        );
        if let Node::Scroll {
            background, border, ..
        } = &mut log
        {
            *background = Some(kit::rgba(p.surface));
            *border = Some(wire::Border {
                color: Some(kit::rgba(p.border)),
                width: Some(1.),
                radius: Some([kit::radius::CARD as f32; 4]),
            });
        }
        content.push(log);
        kit::sized(
            kit::spaced(kit::column("node/activity", content), 12.),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }

    fn section(key: &str, title_key: &str, title: &str, content: Node) -> Node {
        kit::spaced(
            kit::column(key, [kit::heading(title_key, title), content]),
            8.,
        )
    }

    fn reading(key: &str, label: &str, value: Node) -> Node {
        kit::kv(key, label, value)
    }

    fn copyable(key: &str, label: &str, value: &str) -> Node {
        Self::reading(
            key,
            label,
            kit::centered_row(
                format!("{key}/reading"),
                [
                    kit::sized(
                        kit::wrapping(kit::mono(format!("{key}/value"), value)),
                        Some(Length::Fill),
                        None,
                    ),
                    {
                        let mut copy = kit::button(
                            format!("{key}/copy"),
                            "Copy",
                            Some(slots::message(Message::CopyToClipboard(
                                value.into(),
                                format!("{label} copied"),
                            ))),
                            ButtonPreset::Subtle,
                        );
                        if let Node::Button { label: accessible, .. } = &mut copy {
                            *accessible = Some(format!("Copy {}", label.to_lowercase()));
                        }
                        copy
                    },
                ],
            ),
        )
    }
}
