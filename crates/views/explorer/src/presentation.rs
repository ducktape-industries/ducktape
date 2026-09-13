use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{self, ButtonPreset, Length, Node},
};

use crate::{ExplorerView, Message, host};

/// The horizontal inset every flush region shares: a toolbar, a pane header,
/// a list row. Nothing sits on the window edge, and nothing is indented twice.
const GUTTER: wire::Edges = wire::Edges {
    top: 0.,
    right: 12.,
    bottom: 0.,
    left: 12.,
};

impl ExplorerView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let can_refresh = self.connected && !self.loading;
        let toolbar = Self::bar(kit::centered_row(
            "explorer/head",
            [
                kit::nowrap(kit::title("explorer/title", "Explorer")),
                Self::height_badge(self.head),
                kit::nowrap(kit::caption("explorer/sync", &self.sync_line)),
                kit::spacer(),
                kit::sized(
                    kit::input(
                        "explorer/search",
                        "Search messages, pages, issues, files, runs…",
                        &self.query,
                        slots::handler(Box::new(|value: String| Some(Message::BindQuery(value)))),
                        Some(slots::message(Message::SearchSubmit)),
                    ),
                    Some(Length::Fixed(320.)),
                    None,
                ),
                kit::button(
                    "explorer/refresh",
                    "Refresh",
                    can_refresh.then(|| slots::message(Message::Refresh)),
                    ButtonPreset::Secondary,
                ),
            ],
        ));
        let mut body = vec![toolbar, kit::divider("explorer/head-edge")];
        if !self.host_error.is_empty() {
            body.push(kit::padded(
                kit::column(
                    "explorer/error-box",
                    [kit::notice(
                        "explorer/error",
                        kit::wrapping(kit::text("explorer/error-text", &self.host_error)),
                        Tone::Danger,
                    )],
                ),
                wire::Edges::all(12.),
            ));
        }
        let panel = match (self.connected, self.searching, self.sent_query.is_empty()) {
            (false, _, _) => kit::empty_state(
                "explorer/disconnected",
                "Not connected",
                "Choose a network from the sidebar to read its ledger.",
            ),
            (true, true, _) => kit::padded(
                kit::column(
                    "explorer/loading-box",
                    [kit::secondary("explorer/loading-search", "Searching…")],
                ),
                wire::Edges::all(12.),
            ),
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
            child: Box::new(Self::filling(kit::spaced(
                kit::column("explorer/content", body),
                0.,
            ))),
        }
    }

    /// A 40px chrome bar: the toolbar, a pane header, the filter strip.
    fn bar(row: Node) -> Node {
        kit::sized(
            kit::padded(row, GUTTER),
            Some(Length::Fill),
            Some(Length::Fixed(40.)),
        )
    }

    /// A region that takes the whole content area.
    fn filling(node: Node) -> Node {
        kit::sized(node, Some(Length::Fill), Some(Length::Fill))
    }

    /// The head height in the data face, so the digits line up as they move.
    fn height_badge(head: i64) -> Node {
        let mut node = kit::badge("explorer/height", host::height_label(head), Tone::Neutral);
        let Node::Container { content, .. } = &mut node else {
            unreachable!()
        };
        let Node::Text { font, .. } = content.as_mut() else {
            unreachable!()
        };
        font.monospace = true;
        node
    }

    fn ledger(&self) -> Node {
        let mut rows = Vec::new();
        for block in &self.blocks {
            let key = format!("explorer/block/{}", block.height);
            if !rows.is_empty() {
                rows.push(kit::divider(format!("{key}/edge")));
            }
            let line = kit::centered_row(
                format!("{key}/line"),
                [
                    kit::nowrap(kit::weighted(
                        kit::mono(format!("{key}/height"), block.height.to_string()),
                        wire::Weight::Medium,
                    )),
                    kit::sized(
                        kit::nowrap(kit::colored(
                            kit::mono(format!("{key}/hash"), host::hex(&block.hash)),
                            kit::palette().muted,
                        )),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::nowrap(kit::caption(
                        format!("{key}/count"),
                        host::plural(block.op_count, "op", "ops"),
                    )),
                ],
            );
            let selected = block.height == self.selected;
            let mut button = kit::sized(
                kit::list_row(
                    &key,
                    line,
                    selected,
                    Some(slots::message(Message::SelectExplorerBlock(block.height))),
                ),
                Some(Length::Fill),
                Some(Length::Fixed(28.)),
            );
            let Node::Button { label, .. } = &mut button else {
                unreachable!()
            };
            *label = Some("Inspect block".into());
            rows.push(button);
        }
        if rows.is_empty() {
            let plate = match self.loading {
                true => kit::padded(
                    kit::column(
                        "explorer/empty-ledger-box",
                        [kit::secondary("explorer/empty-ledger", "Loading blocks…")],
                    ),
                    wire::Edges::all(12.),
                ),
                false => kit::empty_state(
                    "explorer/empty-ledger",
                    "No blocks yet",
                    "Blocks that carry operations appear here as the network writes them.",
                ),
            };
            rows.push(plate);
        }
        let list = kit::pane(
            "explorer/ledger-pane",
            kit::scroll(
                "explorer/blocks",
                kit::spaced(kit::column("explorer/block-list", rows), 0.),
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
            // the hairline shows; the 10px grip is what the pointer lands on
            content: Box::new(kit::sized(
                kit::container(
                    "explorer/ledger-grip",
                    kit::vertical_divider("explorer/ledger-edge"),
                ),
                Some(Length::Fixed(10.)),
                Some(Length::Fill),
            )),
        };
        Self::filling(kit::spaced(
            kit::row("explorer/ledger", [list, divider, self.block_details()]),
            0.,
        ))
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
        let header = Self::bar(kit::centered_row(
            "explorer/block-head",
            [
                kit::nowrap(kit::heading(
                    "explorer/block-title",
                    format!("Block {}", block.height),
                )),
                kit::spacer(),
                kit::button(
                    "explorer/block-hash/copy",
                    "Copy block hash",
                    Some(slots::message(Message::CopyToClipboard(
                        block.hash.clone(),
                        "Block hash copied".into(),
                    ))),
                    ButtonPreset::Subtle,
                ),
            ],
        ));
        let ops = host::explorer_ops_at(&self.ops, self.selected);
        let mut content = vec![
            kit::kv(
                "explorer/block-hash",
                "Block hash",
                kit::wrapping(kit::mono(
                    "explorer/block-hash/value",
                    host::hex(&block.hash),
                )),
            ),
            Self::digest("explorer/commit", "Commit", &block.commit),
            kit::kv(
                "explorer/block-ops",
                "Operations",
                kit::text(
                    "explorer/block-ops/value",
                    host::plural(block.op_count, "operation", "operations"),
                ),
            ),
        ];
        if !ops.is_empty() {
            content.push(kit::divider("explorer/operations-edge"));
        }
        for (index, op) in ops.iter().enumerate() {
            let key = format!("explorer/operation/{index}");
            let disposition = match op.applied {
                true => Tone::Success,
                false => Tone::Neutral,
            };
            let mut lines = vec![
                Self::operation_line(&key, index, op, disposition),
                Self::proposer(&key, &op.proposer),
                Self::digest(&format!("{key}/hash"), "Op hash", &op.op_hash),
                kit::kv(
                    format!("{key}/payload"),
                    "Payload",
                    Self::payload(&key, &op.payload),
                ),
            ];
            if !op.trace.is_empty() {
                lines.push(kit::kv(
                    format!("{key}/trace"),
                    "Dispatch",
                    Self::trace(&key, &op.trace),
                ));
            }
            content.push(kit::spaced(kit::column(format!("{key}/body"), lines), 4.));
        }
        Self::filling(kit::spaced(
            kit::column(
                "explorer/details-body",
                [
                    header,
                    kit::divider("explorer/block-edge"),
                    kit::scroll(
                        "explorer/details",
                        kit::padded(
                            kit::spaced(kit::column("explorer/detail-content", content), 8.),
                            wire::Edges::all(12.),
                        ),
                    ),
                ],
            ),
            0.,
        ))
    }

    /// One operation of the block: its index, the module it targeted, the
    /// message's verb, and how it landed. The payload itself is the table
    /// under it — never a JSON string on this line.
    fn operation_line(key: &str, index: usize, op: &host::ExplorerOp, disposition: Tone) -> Node {
        kit::sized(
            kit::centered_row(
                format!("{key}/head"),
                [
                    kit::sized(
                        kit::nowrap(kit::colored(
                            kit::mono(format!("{key}/index"), index.to_string()),
                            kit::palette().muted,
                        )),
                        Some(Length::Fixed(18.)),
                        None,
                    ),
                    kit::badge(format!("{key}/target"), &op.target, Tone::Neutral),
                    kit::sized(
                        kit::nowrap(kit::strong(format!("{key}/verb"), op.verb())),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::badge(format!("{key}/status"), &op.disposition, disposition),
                ],
            ),
            Some(Length::Fill),
            Some(Length::Fixed(28.)),
        )
    }

    /// The proposer row: the key or label as words, the copy carrying the
    /// handle as it came.
    fn proposer(key: &str, proposer: &str) -> Node {
        Self::copy_row(
            &format!("{key}/proposer"),
            "Proposer",
            host::proposer_label(proposer),
            proposer,
        )
    }

    /// What the op carried: a table of its fields, or the text as it came
    /// in a code box when it is not a message this can read.
    fn payload(key: &str, payload: &host::Payload) -> Node {
        let fields = match payload {
            host::Payload::Text(text) if text.is_empty() => {
                return kit::secondary(format!("{key}/payload/none"), "Nothing");
            }
            host::Payload::Text(text) => {
                return kit::card(
                    format!("{key}/payload/box"),
                    kit::wrapping(kit::mono(format!("{key}/payload/text"), text)),
                );
            }
            host::Payload::Fields(fields) => fields,
        };
        // the op line already names the verb: a field reads from inside it
        let verb = fields
            .first()
            .and_then(|field| field.name.split('.').next())
            .unwrap_or_default();
        let verb_prefix = format!("{verb}.");
        let rows = fields.iter().enumerate().map(|(index, field)| {
            let row_key = format!("{key}/payload/{index}");
            let is_digest = field.value.starts_with("0x");
            let value = match is_digest {
                true => kit::mono(format!("{row_key}/value"), &field.value),
                false => kit::text(format!("{row_key}/value"), &field.value),
            };
            let name = field.name.strip_prefix(&verb_prefix).unwrap_or(&field.name);
            kit::kv(row_key, name, kit::wrapping(value))
        });
        kit::card(
            format!("{key}/payload/box"),
            kit::spaced(kit::column(format!("{key}/payload/fields"), rows), 4.),
        )
    }

    /// The dispatch trace: one row per module the op reached, naming what it
    /// emitted there.
    fn trace(key: &str, hops: &[host::TraceHop]) -> Node {
        let rows = hops.iter().enumerate().map(|(index, hop)| {
            let row_key = format!("{key}/trace/{index}");
            kit::kv(
                row_key.clone(),
                &hop.module,
                kit::wrapping(kit::text(format!("{row_key}/emitted"), hop.emitted())),
            )
        });
        kit::card(
            format!("{key}/trace/box"),
            kit::spaced(kit::column(format!("{key}/trace/hops"), rows), 4.),
        )
    }

    /// A digest row: the value in the data face, with the copy that hands the
    /// bare key to the clipboard.
    fn digest(key: &str, label: &str, value: &str) -> Node {
        Self::copy_row(key, label, host::hex(value), value)
    }

    /// A labelled value in the data face, with the copy that hands `value`
    /// — not what is shown — to the clipboard.
    fn copy_row(key: &str, label: &str, shown: String, value: &str) -> Node {
        kit::kv(
            key,
            label,
            kit::centered_row(
                format!("{key}/row"),
                [
                    kit::sized(
                        kit::wrapping(kit::mono(format!("{key}/value"), shown)),
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
        )
    }

    fn search_results(&self) -> Node {
        let mut choices = vec![(
            "all".to_owned(),
            "All".to_owned(),
            self.kind == "all",
            Some(slots::message(Message::PickExplorerKind("all".into()))),
        )];
        for kind in &self.kinds {
            choices.push((
                kind.kind.clone(),
                format!("{}  {}", kind.label, kind.count),
                kind.kind == self.kind,
                Some(slots::message(Message::PickExplorerKind(kind.kind.clone()))),
            ));
        }
        let filters = Self::bar(kit::centered_row(
            "explorer/filters",
            [
                kit::sized(
                    kit::tabs("explorer/filter", choices),
                    Some(Length::Shrink),
                    None,
                ),
                kit::spacer(),
                kit::button(
                    "explorer/clear",
                    "Clear workspace search",
                    Some(slots::message(Message::ClearExplorerSearch)),
                    ButtonPreset::Text,
                ),
            ],
        ));
        let mut content = Vec::new();
        if !self.partial.is_empty() {
            content.push(kit::padded(
                kit::column(
                    "explorer/partial-box",
                    [kit::notice(
                        "explorer/partial",
                        kit::wrapping(kit::text("explorer/partial-text", &self.partial)),
                        Tone::Warning,
                    )],
                ),
                wire::Edges::all(12.),
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
        }
        for (index, hit) in hits.iter().enumerate() {
            let key = format!("explorer/result/{index}");
            if index > 0 {
                content.push(kit::divider(format!("{key}/edge")));
            }
            content.push(kit::sized(
                kit::padded(
                    kit::centered_row(
                        format!("{key}/line"),
                        [
                            kit::badge(format!("{key}/kind"), &hit.label, Tone::Neutral),
                            kit::sized(
                                kit::nowrap(kit::strong(format!("{key}/title"), &hit.title)),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::sized(
                                kit::nowrap(kit::secondary(format!("{key}/snippet"), &hit.snippet)),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::nowrap(kit::colored(
                                kit::mono(format!("{key}/place"), &hit.place),
                                kit::palette().muted,
                            )),
                            kit::nowrap(kit::caption(format!("{key}/detail"), &hit.detail)),
                        ],
                    ),
                    GUTTER,
                ),
                Some(Length::Fill),
                Some(Length::Fixed(32.)),
            ));
            // the reference is what a hit is good for: the id to copy
            content.push(kit::padded(
                Self::digest(&format!("{key}/target"), "Reference", &hit.target),
                wire::Edges {
                    top: 0.,
                    right: GUTTER.right,
                    bottom: 6.,
                    left: GUTTER.left,
                },
            ));
        }
        Self::filling(kit::spaced(
            kit::column(
                "explorer/results-body",
                [
                    filters,
                    kit::divider("explorer/filters-edge"),
                    kit::scroll(
                        "explorer/results",
                        kit::spaced(kit::column("explorer/result-list", content), 0.),
                    ),
                ],
            ),
            0.,
        ))
    }
}
