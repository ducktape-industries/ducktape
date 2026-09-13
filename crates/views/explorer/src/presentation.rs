use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{self, ButtonPreset, Length, Node},
};

use crate::{ExplorerView, Message, host};

impl ExplorerView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let search = kit::sized(
            kit::input(
                "explorer/search",
                "Search messages, pages, issues, files, runs…",
                &self.query,
                slots::handler(Box::new(|value: String| Some(Message::BindQuery(value)))),
                Some(slots::message(Message::SearchSubmit)),
            ),
            Some(Length::Fill),
            None,
        );
        let can_refresh = self.connected && !self.loading;
        let mut body = vec![
            kit::centered_row(
                "explorer/head",
                [
                    kit::sized(
                        kit::title("explorer/title", "Explorer"),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::spaced(
                        kit::centered_row(
                            "explorer/status",
                            [
                                kit::badge(
                                    "explorer/height",
                                    host::height_label(self.head),
                                    Tone::Neutral,
                                ),
                                kit::nowrap(kit::secondary("explorer/sync", &self.sync_line)),
                            ],
                        ),
                        8.,
                    ),
                ],
            ),
            kit::spaced(
                kit::row(
                    "explorer/toolbar",
                    [
                        search,
                        kit::button(
                            "explorer/refresh",
                            "Refresh",
                            can_refresh.then(|| slots::message(Message::Refresh)),
                            ButtonPreset::Secondary,
                        ),
                    ],
                ),
                8.,
            ),
        ];
        if !self.host_error.is_empty() {
            body.push(kit::notice(
                "explorer/error",
                kit::wrapping(kit::text("explorer/error-text", &self.host_error)),
                Tone::Danger,
            ));
        }
        let panel = match (self.connected, self.searching, self.sent_query.is_empty()) {
            (false, _, _) => kit::empty_state(
                "explorer/disconnected",
                "Not connected",
                "Choose a network from the sidebar to read its ledger.",
            ),
            (true, true, _) => kit::secondary("explorer/loading-search", "Searching…"),
            (true, false, false) => self.search_results(),
            (true, false, true) => self.ledger(),
        };
        body.push(panel);
        let viewport = slots::handler(Box::new(|(width, height): (f32, f32)| {
            Some(Message::ViewportChanged(
                f64::from(width),
                f64::from(height),
            ))
        }));
        Node::Sensor {
            key: "explorer/viewport".into(),
            reset: None,
            on_show: Some(viewport),
            on_resize: Some(viewport),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(kit::page("explorer/content", body)),
        }
    }

    fn ledger(&self) -> Node {
        let mut rows = Vec::new();
        for block in &self.blocks {
            let key = format!("explorer/block/{}", block.height);
            let summary = kit::spaced(
                kit::column(
                    format!("{key}/summary"),
                    [
                        kit::spaced(
                            kit::centered_row(
                                format!("{key}/metadata"),
                                [
                                    kit::sized(
                                        kit::strong(
                                            format!("{key}/height"),
                                            block.height.to_string(),
                                        ),
                                        Some(Length::Fill),
                                        None,
                                    ),
                                    kit::nowrap(kit::caption(
                                        format!("{key}/count"),
                                        host::plural(block.op_count, "op", "ops"),
                                    )),
                                ],
                            ),
                            8.,
                        ),
                        kit::nowrap(kit::colored(
                            kit::mono(format!("{key}/hash"), host::hex(&block.hash)),
                            kit::palette().muted,
                        )),
                    ],
                ),
                2.,
            );
            let selected = block.height == self.selected;
            let mut button = kit::list_row(
                &key,
                summary,
                selected,
                Some(slots::message(Message::SelectExplorerBlock(block.height))),
            );
            if let Node::Button { label, .. } = &mut button {
                *label = Some("Inspect block".into());
            }
            rows.push(button);
        }
        if rows.is_empty() {
            let message = if self.loading {
                "Loading blocks…"
            } else {
                "No blocks carrying operations yet."
            };
            rows.push(kit::padded(
                kit::column(
                    "explorer/empty-ledger-box",
                    [kit::secondary("explorer/empty-ledger", message)],
                ),
                wire::Edges::all(12.),
            ));
        }
        let list = kit::pane(
            "explorer/ledger-pane",
            kit::scroll(
                "explorer/blocks",
                kit::padded(
                    kit::spaced(kit::column("explorer/block-list", rows), 2.),
                    wire::Edges::all(8.),
                ),
            ),
            Length::Fixed(self.ledger_width as f32),
        );
        let divider = Node::ResizeHandle {
            key: "explorer/ledger-resize".into(),
            on_press: None,
            on_release: None,
            on_drag: Some(slots::handler(Box::new(|(x, y): (f64, f64)| {
                Some(Message::LedgerResized(x, y))
            }))),
            cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
            content: Box::new(kit::vertical_divider("explorer/ledger-edge")),
        };
        let mut frame = kit::card(
            "explorer/ledger-frame",
            kit::sized(
                kit::row("explorer/ledger", [list, divider, self.block_details()]),
                Some(Length::Fill),
                Some(Length::Fill),
            ),
        );
        if let Node::Container {
            padding,
            clip,
            height,
            ..
        } = &mut frame
        {
            *padding = None;
            *clip = true;
            *height = Some(Length::Fill);
        }
        frame
    }

    fn block_details(&self) -> Node {
        let block = self
            .blocks
            .iter()
            .find(|block| block.height == self.selected);
        let Some(block) = block else {
            return kit::empty_state(
                "explorer/selection",
                "Nothing selected",
                "Select a block to inspect its operations and dispatch trace.",
            );
        };
        let mut content = vec![
            kit::heading("explorer/block-title", format!("Block {}", block.height)),
            Self::digest("explorer/block-hash", "Block hash", &block.hash),
            Self::digest("explorer/commit", "Commit", &block.commit),
        ];
        let ops = host::explorer_ops_at(&self.ops, self.selected);
        if !ops.is_empty() {
            content.push(kit::heading(
                "explorer/operations-title",
                host::plural(ops.len() as i64, "operation", "operations"),
            ));
        }
        for (index, op) in ops.iter().enumerate() {
            let key = format!("explorer/operation/{index}");
            let applied = op.disposition.starts_with("applied") || op.disposition == "ok";
            content.push(kit::card(
                &key,
                kit::spaced(
                    kit::column(
                        format!("{key}/body"),
                        [
                            kit::centered_row(
                                format!("{key}/head"),
                                [
                                    kit::sized(
                                        kit::strong(format!("{key}/title"), &op.target),
                                        Some(Length::Fill),
                                        None,
                                    ),
                                    kit::badge(
                                        format!("{key}/status"),
                                        &op.disposition,
                                        if applied {
                                            Tone::Success
                                        } else {
                                            Tone::Neutral
                                        },
                                    ),
                                ],
                            ),
                            Self::digest(&format!("{key}/proposer"), "Proposer", &op.proposer),
                            Self::digest(&format!("{key}/hash"), "Op hash", &op.op_hash),
                            kit::wrapping(kit::mono(format!("{key}/payload"), &op.payload)),
                            kit::wrapping(kit::caption(format!("{key}/trace"), &op.trace)),
                        ],
                    ),
                    8.,
                ),
            ));
        }
        kit::scroll(
            "explorer/details",
            kit::padded(
                kit::spaced(kit::column("explorer/detail-content", content), 12.),
                wire::Edges::all(16.),
            ),
        )
    }

    fn digest(key: &str, label: &str, value: &str) -> Node {
        kit::spaced(
            kit::column(
                key,
                [
                    kit::centered_row(
                        format!("{key}/header"),
                        [
                            kit::sized(
                                kit::label(format!("{key}/label"), label),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::button(
                                format!("{key}/copy"),
                                format!("Copy {}", label.to_lowercase()),
                                Some(slots::message(Message::CopyToClipboard(
                                    value.into(),
                                    format!("{label} copied"),
                                ))),
                                ButtonPreset::Subtle,
                            ),
                        ],
                    ),
                    kit::wrapping(kit::mono(format!("{key}/value"), host::hex(value))),
                ],
            ),
            2.,
        )
    }

    fn search_results(&self) -> Node {
        let mut filters = vec![{
            let mut all = kit::button(
                "explorer/filter/all",
                "All",
                Some(slots::message(Message::PickExplorerKind("all".into()))),
                ButtonPreset::Subtle,
            );
            if let Node::Button { checked, .. } = &mut all {
                *checked = Some(self.kind == "all");
            }
            all
        }];
        for kind in &self.kinds {
            let selected = kind.kind == self.kind;
            let mut button = kit::button(
                format!("explorer/filter/{}", kind.kind),
                format!("{}  {}", kind.label, kind.count),
                Some(slots::message(Message::PickExplorerKind(kind.kind.clone()))),
                ButtonPreset::Subtle,
            );
            if let Node::Button { checked, .. } = &mut button {
                *checked = Some(selected);
            }
            filters.push(button);
        }
        filters.push(kit::spacer());
        filters.push(kit::button(
            "explorer/clear",
            "Clear workspace search",
            Some(slots::message(Message::ClearExplorerSearch)),
            ButtonPreset::Text,
        ));
        let mut content = vec![kit::spaced(
            kit::centered_row("explorer/filters", filters),
            2.,
        )];
        if !self.partial.is_empty() {
            content.push(kit::notice(
                "explorer/partial",
                kit::wrapping(kit::text("explorer/partial-text", &self.partial)),
                Tone::Warning,
            ));
        }
        let matches_kind = |hit: &&host::ExplorerHit| self.kind == "all" || hit.kind == self.kind;
        let hits: Vec<_> = self.hits.iter().filter(matches_kind).collect();
        let empty_answer = hits.is_empty()
            && self.partial.is_empty()
            && host::search_answer_stands(&self.sent_query, &self.query, self.searching);
        if empty_answer {
            content.push(kit::empty_state(
                "explorer/no-results-state",
                "No matching results.",
                "Try another word, or clear the filter above.",
            ));
            if let Some(Node::Linear { children, .. }) = content.last_mut()
                && let Some(Node::Text { key, .. }) = children.first_mut()
            {
                *key = "explorer/no-results".into();
            }
        }
        for (index, hit) in hits.iter().enumerate() {
            let key = format!("explorer/result/{index}");
            content.push(kit::card(
                &key,
                kit::spaced(
                    kit::column(
                        format!("{key}/body"),
                        [
                            kit::centered_row(
                                format!("{key}/head"),
                                [
                                    kit::sized(
                                        kit::wrapping(kit::strong(
                                            format!("{key}/title"),
                                            &hit.title,
                                        )),
                                        Some(Length::Fill),
                                        None,
                                    ),
                                    kit::badge(format!("{key}/kind"), &hit.meta, Tone::Neutral),
                                ],
                            ),
                            kit::wrapping(kit::secondary(format!("{key}/snippet"), &hit.snippet)),
                            Self::digest(&format!("{key}/target"), "Reference", &hit.target),
                        ],
                    ),
                    8.,
                ),
            ));
        }
        kit::scroll(
            "explorer/results",
            kit::spaced(kit::column("explorer/result-list", content), 12.),
        )
    }
}
