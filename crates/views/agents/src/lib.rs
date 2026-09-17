//! The Agents register as a view on the kernel contract: who may act, which
//! executor they run on, the skills they carry and what their runs did,
//! rendered from a wasm component the desktop app loads from a file.
//!
//! The kernel pushes session facts only (`agents.props`: connected, dark,
//! the signing account, and the run another tab opened for the reader). The
//! register, the run tracker and one run's journal are read here through
//! the kernel's `rpc.query` / `rpc.view`, re-read on every `rpc.live` hit
//! for the `runs` and `identity` planes, and a pause or a save leaves as
//! `op.submit` — the runs message the kernel signs with the seated key. The
//! endpoint, the key and the password never cross: a guest that sees no key
//! cannot leak one.
pub mod host;
use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
    wire::{self, ButtonPreset, Length, Node},
};

fn action(key: impl Into<String>, label: &str, message: Option<Message>) -> Node {
    kit::button(
        key,
        label,
        message.map(slots::message),
        ButtonPreset::Secondary,
    )
}

fn primary(key: impl Into<String>, label: &str, message: Option<Message>) -> Node {
    kit::button(
        key,
        label,
        message.map(slots::message),
        ButtonPreset::Primary,
    )
}

fn subtle(key: impl Into<String>, label: &str, message: Option<Message>) -> Node {
    let key = key.into();
    let mut button = kit::button_child(
        &key,
        kit::sized(
            kit::wrapping(kit::text(format!("{key}/label"), label)),
            Some(Length::Fill),
            None,
        ),
        message.map(slots::message),
        ButtonPreset::Subtle,
    );
    if let Node::Button {
        label: accessible, ..
    } = &mut button
    {
        *accessible = Some(label.to_owned());
    }
    button
}

fn field(key: &str, hint: &str, value: &str, message: fn(String) -> Message) -> Node {
    kit::sized(
        kit::input(
            key,
            hint,
            value,
            slots::handler(Box::new(move |value| Some(message(value)))),
            None,
        ),
        Some(Length::Fill),
        None,
    )
}

fn places(key: &str, links: &[host::RunLink], opened: Option<&str>) -> Node {
    kit::spaced(
        kit::column(
            key,
            links.iter().enumerate().map(|(index, link)| {
                let preview_key = format!("{key}/{}", link.url);
                let key = format!("{key}/{index}");
                if link.url.is_empty() {
                    return kit::badge(key, &link.label, Tone::Neutral);
                }
                let Some(preview) = &link.preview else {
                    let mut button = kit::button_child(
                        &key,
                        kit::wrapping(kit::text(format!("{key}/label"), &link.label)),
                        Some(slots::message(Message::OpenPlace(link.url.clone()))),
                        ButtonPreset::Text,
                    );
                    if let Node::Button { label, .. } = &mut button {
                        *label = Some(link.label.clone());
                    }
                    return kit::sized(button, Some(Length::Fill), None);
                };
                let expanded = opened == Some(preview_key.as_str());
                message_chip(&key, link, preview, expanded, preview_key)
            }),
        ),
        6.,
    )
}

/// A chat message's chip: a card headed by the message's address with the
/// disclosure at its right edge; opened, the message reads as a quote —
/// author over body — with the way into the room as a proper button.
fn message_chip(
    key: &str,
    link: &host::RunLink,
    preview: &host::MessagePreview,
    expanded: bool,
    preview_key: String,
) -> Node {
    let open = || Some(Message::OpenPlace(link.url.clone()));
    let address = kit::button(
        key,
        &link.label,
        open().map(slots::message),
        ButtonPreset::Text,
    );
    let toggle = kit::button(
        format!("{key}/toggle"),
        if expanded {
            "Hide message"
        } else {
            "View message"
        },
        Some(slots::message(Message::ToggleMessagePreview(preview_key))),
        ButtonPreset::Subtle,
    );
    let header = kit::sized(
        kit::centered_row(
            format!("{key}/header"),
            [address, kit::space(Some(Length::Fill), None), toggle],
        ),
        Some(Length::Fill),
        None,
    );
    let mut content = vec![header];
    if expanded {
        let mut quote = Vec::new();
        if !preview.author.is_empty() {
            quote.push(kit::label(format!("{key}/author"), &preview.author));
        }
        quote.push(kit::wrapping(kit::text(
            format!("{key}/preview"),
            &preview.body,
        )));
        content.push(kit::spaced(kit::column(format!("{key}/quote"), quote), 4.));
        content.push(kit::row(
            format!("{key}/actions"),
            [action(format!("{key}/open-chat"), "Open in chat", open())],
        ));
    }
    kit::card(
        format!("{key}/message"),
        kit::spaced(kit::column(format!("{key}/body"), content), 8.),
    )
}

fn resize(key: &str, message: fn(f64, f64) -> Message) -> Node {
    Node::ResizeHandle {
        key: key.into(),
        on_press: None,
        on_release: None,
        on_drag: Some(slots::handler(Box::new(move |(x, y)| Some(message(x, y))))),
        cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
        content: Box::new(kit::vertical_divider(format!("{key}/edge"))),
    }
}

/// The tone a run or agent state paints: green settled, red failed,
/// amber in flight, grey otherwise. The accent is not a state colour.
fn state_tone(state: &str) -> Tone {
    let word = state.split(['·', ' ']).next().unwrap_or_default().trim();
    match word.to_ascii_lowercase().as_str() {
        "active" | "done" | "settled" | "merged" | "succeeded" | "accepted" | "completed" => {
            Tone::Success
        }
        "failed" | "error" | "refused" | "rejected" => Tone::Danger,
        "running" | "working" | "dispatched" | "pending" | "queued" => Tone::Warning,
        _ => Tone::Neutral,
    }
}

/// A state word as a badge spells it: `failed` reads `Failed`.
fn sentence(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A state badge: the word sentence-cased, toned by what it says.
fn state_badge(key: impl Into<String>, state: &str) -> Node {
    kit::badge(key, sentence(state), state_tone(state))
}

/// What a failure strip says above the reason a run settled with.
fn settled_title(state: &str) -> &'static str {
    match state {
        "rejected" => "This run's result was rejected",
        "failed" => "This run failed",
        _ => "This run stopped",
    }
}

fn section(key: &str, title: &str, children: impl IntoIterator<Item = Node>) -> Node {
    let mut items = vec![kit::heading(format!("{key}/title"), title)];
    items.extend(children);
    kit::spaced(kit::column(key, items), 8.)
}

/// A height or a time in the data face, at caption weight: the rail every
/// journal entry is stamped with.
fn stamp(key: String, content: &str) -> Node {
    kit::nowrap(kit::colored(
        kit::text_size(kit::mono(key, content), kit::type_scale::CAPTION as f32),
        kit::palette().muted,
    ))
}

/// The gap that pushes what follows to the right edge of its row.
fn filler() -> Node {
    kit::space(Some(Length::Fill), None)
}

/// The detail rail beside a list: the reader's width, the window's own
/// colour, clipped at its edge.
fn detail_pane(key: &str, content: Node, width: f64) -> Node {
    let mut node = kit::container(key, content);
    let Node::Container {
        width: value,
        height,
        clip,
        ..
    } = &mut node
    else {
        unreachable!()
    };
    *value = Some(Length::Fixed(width as f32));
    *height = Some(Length::Fill);
    *clip = true;
    node
}

impl AgentsView {
    fn view(&self) -> Node {
        kit::set_dark(self.dark);
        let mut content = vec![self.toolbar(), kit::divider("agents/toolbar-rule")];
        let note = host::pane_note(&self.panel);
        if !note.is_empty() {
            content.push(kit::padded(
                kit::row(
                    "agents/about-row",
                    [kit::wrapping(kit::secondary("agents/about", note))],
                ),
                wire::Edges {
                    top: 8.,
                    right: 12.,
                    bottom: 8.,
                    left: 12.,
                },
            ));
            content.push(kit::divider("agents/about-rule"));
        }
        if !self.host_error.is_empty() {
            content.push(kit::padded(
                kit::row(
                    "agents/error-row",
                    [kit::notice(
                        "agents/error",
                        kit::wrapping(kit::text("agents/error-text", &self.host_error)),
                        Tone::Danger,
                    )],
                ),
                wire::Edges::all(12.),
            ));
        }
        content.push(match self.connected {
            false => kit::empty_state(
                "agents/disconnected",
                "Not connected",
                "Choose a network to read its agent registry.",
            ),
            true => match self.panel.as_str() {
                "runs" => self.runs_panel(),
                _ => self.registry_panel(),
            },
        });
        let viewport = slots::handler::<(f32, f32), Message>(Box::new(|(w, h)| {
            Some(Message::ViewportChanged(w.into(), h.into()))
        }));
        Node::Sensor {
            key: "agents/viewport".into(),
            reset: None,
            on_show: Some(viewport),
            on_resize: Some(viewport),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(kit::sized(
                kit::spaced(kit::column("agents/root", content), 0.),
                Some(Length::Fill),
                Some(Length::Fill),
            )),
        }
    }

    /// The screen's own toolbar: what this view is, which pane is on, what
    /// the pane counts, and the one action the register offers.
    fn toolbar(&self) -> Node {
        let mut items = vec![kit::nowrap(kit::title("agents/title", "Agents"))];
        if self.connected {
            let panels = [("registry", "Registry"), ("runs", "Runs")].map(|(panel, label)| {
                (
                    panel.to_owned(),
                    label.to_owned(),
                    self.panel == panel,
                    Some(slots::message(Message::ChoosePanel(panel.to_owned()))),
                )
            });
            items.push(kit::sized(
                kit::tabs("agents/panel", panels),
                Some(Length::Shrink),
                None,
            ));
        }
        items.push(filler());
        if self.connected {
            let summary = match self.panel.as_str() {
                "runs" => host::runs_summary(&self.runs),
                _ => host::agents_summary(self.connected, &self.rows),
            };
            if !summary.is_empty() {
                items.push(kit::nowrap(kit::secondary("agents/summary", summary)));
            }
            if !self.account.is_empty() {
                items.push(primary("agents/new", "New agent", Some(Message::OpenNew)));
            }
        }
        kit::sized(
            kit::padded(
                kit::centered_row("agents/toolbar", items),
                wire::Edges {
                    top: 0.,
                    right: 12.,
                    bottom: 0.,
                    left: 12.,
                },
            ),
            Some(Length::Fill),
            Some(Length::Fixed(40.)),
        )
    }

    /// A list beside a rail, flush to the edges of the content area: the
    /// list is the surface pane, the rail keeps the width the reader
    /// dragged it to, and a hairline is all that stands between them.
    fn split(&self, key: &str, list: Node, rail: Option<(Node, Node)>) -> Node {
        let mut panes = vec![kit::pane(format!("{key}/list"), list, Length::Fill)];
        if let Some((handle, pane)) = rail {
            panes.push(handle);
            panes.push(pane);
        }
        kit::sized(
            kit::spaced(kit::row(key, panes), 0.),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }

    fn registry_panel(&self) -> Node {
        let mut rows = Vec::new();
        let no_agents = self.rows.is_empty() && !self.creating;
        match (self.answered, no_agents) {
            (false, _) => rows.push(kit::padded(
                kit::row(
                    "agents/loading-row",
                    [kit::secondary("agents/loading", "Reading the registry…")],
                ),
                wire::Edges::all(16.),
            )),
            (true, true) => rows.push(kit::empty_state(
                "agents/empty",
                "No agents registered",
                "Agents appear here with their executor and skills once someone registers one.",
            )),
            (true, false) => {}
        }
        for agent in &self.rows {
            let key = format!("agents/record/{}", agent.id);
            let owner = if agent.owner_handle.is_empty() {
                "unowned"
            } else {
                &agent.owner_handle
            };
            let identity = kit::spaced(
                kit::column(
                    format!("{key}/identity"),
                    [
                        kit::nowrap(kit::strong(format!("{key}/name"), &agent.name)),
                        kit::nowrap(kit::text_size(
                            kit::secondary(
                                format!("{key}/meta"),
                                format!(
                                    "{} · {} · {}",
                                    owner,
                                    agent.capability,
                                    host::plural(agent.skills.len() as i64, "skill", "skills")
                                ),
                            ),
                            12.,
                        )),
                    ],
                ),
                4.,
            );
            let mut line = vec![
                kit::avatar(
                    format!("{key}/avatar"),
                    kit::initials(&agent.name),
                    Tone::Agent,
                ),
                kit::sized(identity, Some(Length::Fill), None),
            ];
            if agent.live {
                line.push(kit::badge(format!("{key}/working"), "Working", Tone::Agent));
            }
            line.push(state_badge(format!("{key}/standing"), &agent.status));
            let summary = kit::spaced(kit::centered_row(format!("{key}/summary"), line), 10.);
            let mut button = kit::list_row(
                &key,
                summary,
                self.selected == agent.id,
                Some(slots::message(Message::OpenAgent(agent.id.clone()))),
            );
            if let Node::Button { label, padding, .. } = &mut button {
                *padding = Some(wire::Edges {
                    top: 10.,
                    right: 10.,
                    bottom: 10.,
                    left: 10.,
                });
                *label = Some(agent.name.clone());
            }
            rows.push(button);
        }
        let list = kit::scroll(
            "agents/registry-scroll",
            kit::padded(
                kit::spaced(kit::column("agents/registry", rows), 2.),
                wire::Edges::all(8.),
            ),
        );
        let editor_open = !self.selected.is_empty() || self.creating;
        let rail = editor_open.then(|| {
            (
                resize("agents/editor-resize", Message::EditorResized),
                detail_pane(
                    "agents/editor",
                    kit::scroll("agents/editor-scroll", self.editor()),
                    self.editor_width,
                ),
            )
        });
        self.split("agents/registry-panes", list, rail)
    }

    fn runs_panel(&self) -> Node {
        let mut rows = Vec::new();
        match (self.answered, self.runs.is_empty()) {
            (false, _) => rows.push(kit::padded(
                kit::row(
                    "agents/runs-loading-row",
                    [kit::secondary("agents/runs-loading", "Reading the runs…")],
                ),
                wire::Edges::all(16.),
            )),
            (true, true) => rows.push(kit::empty_state(
                "agents/no-runs",
                "No runs yet",
                "Every dispatch of an agent lands here with its journal.",
            )),
            (true, false) => {}
        }
        for run in &self.runs {
            let key = format!("agents/run/{}", run.run_id);
            let summary = kit::spaced(
                kit::column(
                    format!("{key}/summary"),
                    [
                        kit::spaced(
                            kit::centered_row(
                                format!("{key}/heading"),
                                [
                                    kit::sized(
                                        kit::nowrap(kit::strong(
                                            format!("{key}/agent"),
                                            &run.agent_name,
                                        )),
                                        Some(Length::Fill),
                                        None,
                                    ),
                                    state_badge(format!("{key}/state"), &run.state),
                                ],
                            ),
                            6.,
                        ),
                        kit::sized(
                            kit::nowrap(kit::text_size(
                                kit::secondary(format!("{key}/origin"), &run.origin),
                                12.,
                            )),
                            Some(Length::Fill),
                            None,
                        ),
                        kit::text_size(
                            kit::secondary(
                                format!("{key}/dispatched"),
                                format!("Dispatched at · {}", run.dispatched),
                            ),
                            11.,
                        ),
                    ],
                ),
                4.,
            );
            let mut button = kit::list_row(
                &key,
                summary,
                self.open_run == run.dispatch_id,
                Some(slots::message(Message::OpenRunRow(run.run_id.clone()))),
            );
            if let Node::Button { label, padding, .. } = &mut button {
                *padding = Some(wire::Edges {
                    top: 10.,
                    right: 10.,
                    bottom: 10.,
                    left: 10.,
                });
                *label = Some(run.run_id.clone());
            }
            rows.push(button);
        }
        let list = kit::scroll(
            "agents/runs-scroll",
            kit::padded(
                kit::spaced(kit::column("agents/runs", rows), 2.),
                wire::Edges::all(8.),
            ),
        );
        let detail = match self.open_run.is_empty() {
            true => kit::empty_state(
                "agents/select-run",
                "Select a run",
                "Open a run to see its conversation, work and controls.",
            ),
            false => kit::sized(
                kit::spaced(
                    kit::column(
                        "agents/journal-layout",
                        [
                            kit::scroll("agents/journal-scroll", self.journal_panel()),
                            self.run_controls(),
                        ],
                    ),
                    0.,
                ),
                Some(Length::Fill),
                Some(Length::Fill),
            ),
        };
        kit::sized(
            kit::spaced(
                kit::row(
                    "agents/run-panes",
                    [
                        kit::pane(
                            "agents/run-list",
                            list,
                            Length::Fixed(self.run_list_width as f32),
                        ),
                        resize("agents/run-list-resize", Message::RunListResized),
                        kit::pane("agents/journal", detail, Length::Fill),
                    ],
                ),
                0.,
            ),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }

    fn run_controls(&self) -> Node {
        let Some(control) = &self.live.control else {
            let content = (self.open_row.state == "running").then(|| {
                kit::wrapping(kit::secondary(
                    "agents/control-unavailable",
                    "This session is not connected for run control.",
                ))
            });
            return kit::column("agents/no-controls", content);
        };
        let sending = matches!(self.control_state, host::ControlState::Sending);
        let running = self.open_row.state == "running";
        let can_send = running
            && control.steers
            && !sending
            && !self.control_draft.trim().is_empty()
            && self.control_draft.len() <= 8192;
        let mut content = vec![];
        for (id, detail) in control.approvals.iter().take(1) {
            content.push(kit::sized(
                kit::scroll(
                    format!("agents/approval/{id}/scroll"),
                    kit::wrapping(kit::text(format!("agents/approval/{id}"), detail)),
                ),
                Some(Length::Fill),
                Some(Length::Fixed(120.)),
            ));
            content.push(kit::row(
                format!("agents/approval/{id}/actions"),
                [
                    action(
                        format!("agents/approval/{id}/allow"),
                        "Allow",
                        (!sending).then(|| Message::ControlApprove(id.clone(), true)),
                    ),
                    action(
                        format!("agents/approval/{id}/deny"),
                        "Decline",
                        (!sending).then(|| Message::ControlApprove(id.clone(), false)),
                    ),
                ],
            ));
        }
        if control.steers {
            content.push(kit::input(
                "agents/control-input",
                "Add instructions to this run…",
                &self.control_draft,
                slots::handler(Box::new(|text| Some(Message::ControlDraft(text)))),
                can_send.then(|| slots::message(Message::ControlSend)),
            ));
        }
        content.push(kit::wrapped_row(
            "agents/control-actions",
            [
                primary(
                    "agents/control-send",
                    "Send instructions",
                    can_send.then_some(Message::ControlSend),
                ),
                action(
                    "agents/control-stop",
                    "Stop run",
                    (running && !sending).then_some(Message::ControlInterrupt),
                ),
            ],
        ));
        match &self.control_state {
            host::ControlState::Idle => {}
            host::ControlState::Sending => {
                content.push(kit::secondary("agents/control-status", "Sending…"))
            }
            host::ControlState::Accepted => content.push(kit::secondary(
                "agents/control-status",
                "Received by the session",
            )),
            host::ControlState::Failed(error) => content.push(kit::wrapping(kit::tone_text(
                "agents/control-status",
                error,
                Tone::Danger,
            ))),
        }
        kit::padded(
            kit::column("agents/control-composer", content),
            wire::Edges::all(12.),
        )
    }

    fn markdown(&self, key: &str, text: &str) -> Node {
        Node::Surface {
            key: key.into(),
            name: "markdown".into(),
            args: vec![
                wire::SurfaceValue::Str(text.into()),
                wire::SurfaceValue::Str(String::new()),
                wire::SurfaceValue::Bool(self.dark),
            ],
            on_event: Some(slots::handler::<wire::SurfaceValue, Message>(Box::new(
                |value| match value {
                    wire::SurfaceValue::Str(link) => Some(Message::OpenPlace(link)),
                    _ => None,
                },
            ))),
        }
    }

    fn journal_panel(&self) -> Node {
        let mut items = vec![
            kit::centered_row(
                "agents/journal-heading",
                [
                    kit::sized(
                        kit::wrapping(kit::heading(
                            "agents/journal-run",
                            host::run_title(&self.open_row),
                        )),
                        Some(Length::Fill),
                        None,
                    ),
                    subtle("agents/close-journal", "Close run", Some(Message::CloseRun)),
                ],
            ),
            kit::spaced(
                kit::wrapped_row(
                    "agents/journal-standing",
                    [
                        state_badge("agents/journal-state", &self.open_row.state),
                        kit::wrapping(kit::secondary(
                            "agents/journal-origin",
                            &self.open_row.origin,
                        )),
                        filler(),
                        subtle(
                            "agents/receipt",
                            "Run details",
                            Some(Message::ToggleReceipt(self.open_run.clone())),
                        ),
                    ],
                ),
                8.,
            ),
            kit::secondary(
                "agents/journal-dispatched",
                format!("Dispatched at · {}", self.open_row.dispatched),
            ),
        ];
        if self.expanded_receipt == self.open_run {
            let facts = host::run_facts(&self.open_row).into_iter().enumerate().map(
                |(index, (name, value, code))| {
                    let key = format!("agents/fact/{index}");
                    let text = match code {
                        true => kit::mono(format!("{key}/value"), value),
                        false => kit::text(format!("{key}/value"), value),
                    };
                    kit::kv(key, name, kit::wrapping(text))
                },
            );
            items.push(kit::card(
                "agents/receipt-card",
                kit::spaced(kit::column("agents/receipt-body", facts), 6.),
            ));
        }
        if let host::OutputConnection::Failed(error) = &self.live.connection {
            items.push(kit::notice(
                "agents/output-error",
                kit::column(
                    "agents/output-error-body",
                    [
                        kit::wrapping(kit::text("agents/output-error-text", error)),
                        action(
                            "agents/output-retry",
                            "Reconnect",
                            Some(Message::RetryTrace),
                        ),
                    ],
                ),
                Tone::Danger,
            ));
        }
        items.push(kit::spaced(
            kit::row(
                "agents/run-tabs",
                RunTab::ALL.into_iter().map(|tab| {
                    kit::button(
                        format!("agents/run-tab/{}", tab.label()),
                        tab.label(),
                        Some(slots::message(Message::ChooseRunTab(tab))),
                        if self.run_tab == tab {
                            ButtonPreset::Secondary
                        } else {
                            ButtonPreset::Subtle
                        },
                    )
                }),
            ),
            6.,
        ));
        let content = match self.run_tab {
            RunTab::Conversation => self.conversation_panel(),
            RunTab::Trace => self.trace_panel(),
            RunTab::Journal => self.journal_entries_panel(),
            RunTab::Raw => self.raw_panel(),
        };
        items.push(content);
        kit::spaced(
            kit::padded(
                kit::column("agents/journal-content", items),
                wire::Edges::all(16.),
            ),
            14.,
        )
    }

    fn trace_panel(&self) -> Node {
        let mut items = Vec::new();
        let process_label = self.live.process_label(self.open_row.state == "running");
        items.push(subtle(
            "agents/trace-toggle",
            &format!(
                "{} {process_label}",
                if self.trace_open { "▾" } else { "▸" }
            ),
            Some(Message::ToggleTrace),
        ));
        if self.trace_open {
            let mut steps = Vec::new();
            if self.live.process.is_empty() {
                steps.push(kit::wrapping(kit::secondary(
                    "agents/process-empty",
                    self.live.empty_process_message(&self.open_row.state),
                )));
            }
            for (index, step) in self.live.process.iter().enumerate() {
                let key = format!("agents/process/{index}");
                let (mark, tone, suffix) = match step.state {
                    host::ProcessState::Running => ("·", Tone::Neutral, ""),
                    host::ProcessState::Completed => ("✓", Tone::Neutral, ""),
                    host::ProcessState::Failed => ("!", Tone::Danger, " · failed"),
                };
                let mut content = vec![kit::wrapping(kit::tone_text(
                    format!("{key}/title"),
                    format!("{mark} {}{suffix}", step.title),
                    tone,
                ))];
                if !step.body.is_empty() {
                    let body = match step.code {
                        true => kit::wrapping(kit::mono(format!("{key}/body"), &step.body)),
                        false => self.markdown(&format!("{key}/body"), &step.body),
                    };
                    content.push(body);
                }
                steps.push(kit::card(
                    &key,
                    kit::spaced(kit::column(format!("{key}/content"), content), 6.),
                ));
            }
            steps.push(kit::wrapping(kit::caption(
                "agents/process-retention",
                "Recent output retained by this node",
            )));
            items.push(kit::spaced(kit::column("agents/process", steps), 8.));
        }
        kit::spaced(kit::column("agents/trace-panel", items), 12.)
    }

    fn conversation_panel(&self) -> Node {
        let mut items = vec![subtle(
            "agents/conversation-work",
            &self.live.process_label(self.open_row.state == "running"),
            Some(Message::ChooseRunTab(RunTab::Trace)),
        )];
        if !self.live.answer.is_empty() {
            items.push(self.markdown(
                "agents/answer-full",
                &host::answer_markdown(&self.live.answer),
            ));
        }
        let show_progress =
            self.live.present && self.live.answer.is_empty() && self.live.elapsed_ms.is_none();
        if show_progress {
            let mut live = vec![kit::strong("agents/live-state", &self.live.status)];
            for (index, activity) in self.live.activity.iter().enumerate() {
                live.push(kit::spaced(
                    kit::centered_row(
                        format!("agents/activity/{index}"),
                        [
                            kit::tone_text(
                                format!("agents/activity/{index}/state"),
                                if activity.done { "✓" } else { "…" },
                                if activity.done {
                                    Tone::Success
                                } else {
                                    Tone::Warning
                                },
                            ),
                            kit::wrapping(kit::text(
                                format!("agents/activity/{index}/label"),
                                &activity.label,
                            )),
                        ],
                    ),
                    8.,
                ));
            }
            if !self.live.answer_preview.is_empty() {
                live.push(kit::wrapping(kit::secondary(
                    "agents/answer",
                    &self.live.answer_preview,
                )));
            }
            items.push(kit::notice(
                "agents/live",
                kit::spaced(kit::column("agents/live-body", live), 6.),
                Tone::Agent,
            ));
        }
        if self.live.answer.is_empty() && !show_progress {
            items.push(kit::secondary(
                "agents/conversation-empty",
                "No reply is available for this run.",
            ));
        }
        kit::spaced(kit::column("agents/conversation-panel", items), 14.)
    }

    fn journal_entries_panel(&self) -> Node {
        let mut items = Vec::new();
        let journal_ready = self.journal.dispatch_id == self.open_run;
        if journal_ready && !self.journal.links.is_empty() {
            items.push(section(
                "agents/relevant",
                "Relevant",
                [places(
                    "agents/places",
                    &self.journal.links,
                    self.message_preview_open.as_deref(),
                )],
            ));
        }
        if !self.open_row.reason.is_empty() {
            items.push(kit::notice(
                "agents/run-reason-box",
                kit::spaced(
                    kit::column(
                        "agents/run-reason-body",
                        [
                            kit::strong(
                                "agents/run-reason-title",
                                settled_title(&self.open_row.state),
                            ),
                            kit::wrapping(kit::text("agents/run-reason", &self.open_row.reason)),
                        ],
                    ),
                    4.,
                ),
                Tone::Danger,
            ));
        }
        let mut entries = Vec::new();
        if !journal_ready {
            entries.push(kit::secondary(
                "agents/journal-loading",
                "Reading the journal…",
            ));
        } else if self.journal.entries.is_empty() {
            entries.push(kit::wrapping(kit::secondary(
                "agents/journal-empty",
                "This run has no journal entries yet. The node may still be catching up.",
            )));
        } else {
            for (index, entry) in self.journal.entries.iter().enumerate() {
                let key = format!("agents/entry/{index}");
                let mut head = vec![kit::sized(
                    kit::strong(format!("{key}/kind"), &entry.kind),
                    Some(Length::Fill),
                    None,
                )];
                // a plain fact carries no status: no empty pill for it
                if !entry.status.is_empty() {
                    head.push(state_badge(format!("{key}/status"), &entry.status));
                }
                let mut lines = vec![kit::spaced(
                    kit::centered_row(format!("{key}/head"), head),
                    8.,
                )];
                if !entry.summary.is_empty() {
                    lines.push(kit::wrapping(kit::text(
                        format!("{key}/summary"),
                        &entry.summary,
                    )));
                }
                if !entry.targets.is_empty() {
                    lines.push(places(
                        &format!("{key}/targets"),
                        &entry.targets,
                        self.message_preview_open.as_deref(),
                    ));
                }
                let body = kit::spaced(kit::column(format!("{key}/body"), lines), 4.);
                entries.push(kit::spaced(
                    kit::row(
                        &key,
                        [
                            kit::sized(
                                stamp(format!("{key}/height"), &entry.height),
                                Some(Length::Fixed(72.)),
                                None,
                            ),
                            body,
                        ],
                    ),
                    8.,
                ));
            }
        }
        items.push(section("agents/journal-title-section", "Journal", entries));
        kit::spaced(kit::column("agents/journal-panel", items), 14.)
    }

    fn raw_panel(&self) -> Node {
        let mut items = vec![kit::secondary(
            "agents/raw-count",
            format!("{} retained events", self.live.trace.len()),
        )];
        for (index, raw) in self.live.trace.iter().enumerate() {
            let event = serde_json::from_str::<serde_json::Value>(raw).ok();
            let kind = event
                .as_ref()
                .and_then(|event| event["method"].as_str().or_else(|| event["type"].as_str()))
                .unwrap_or("event");
            let expanded = self.raw_event_open.as_deref() == Some(raw.as_str());
            let key = format!("agents/raw/{index}");
            let label = format!(
                "{} {} · {kind}",
                if expanded { "▾" } else { "▸" },
                index + 1
            );
            let mut toggle = kit::button_child(
                format!("{key}/toggle"),
                kit::sized(
                    kit::wrapping(kit::text(format!("{key}/label"), &label)),
                    Some(Length::Fill),
                    None,
                ),
                Some(slots::message(Message::ToggleRawEvent(raw.clone()))),
                ButtonPreset::Subtle,
            );
            if let Node::Button {
                label: accessible, ..
            } = &mut toggle
            {
                *accessible = Some(label);
            }
            let mut content = vec![toggle];
            if expanded {
                let body = event
                    .as_ref()
                    .map(|event| serde_json::to_string_pretty(event).unwrap_or_default())
                    .unwrap_or_else(|| raw.clone());
                content.push(
                    self.markdown(&format!("{key}/body"), &format!("~~~~json\n{body}\n~~~~")),
                );
            }
            content.push(kit::divider(format!("{key}/divider")));
            items.push(kit::spaced(
                kit::column(format!("{key}/content"), content),
                6.,
            ));
        }
        kit::spaced(kit::column("agents/raw-panel", items), 4.)
    }

    fn editor(&self) -> Node {
        let mut items = vec![kit::centered_row(
            "agents/editor-heading",
            [
                kit::sized(
                    kit::wrapping(kit::heading(
                        "agents/editor-title",
                        if self.creating {
                            "New agent"
                        } else {
                            &self.draft_name
                        },
                    )),
                    Some(Length::Fill),
                    None,
                ),
                subtle(
                    "agents/close-editor",
                    "Close editor",
                    Some(Message::CloseEditor),
                ),
            ],
        )];
        let read_only = !self.can_edit && !self.creating;
        if read_only {
            items.push(kit::notice(
                "agents/read-only-box",
                kit::wrapping(kit::text(
                    "agents/read-only",
                    "Only this agent's controller can change its record. You are reading it.",
                )),
                Tone::Neutral,
            ));
        }
        if !self.creating {
            let mut standing = vec![state_badge("agents/status", &self.selected_status)];
            if self.can_edit {
                match self.selected_status.as_str() {
                    "active" => standing.push(action(
                        "agents/pause",
                        "Pause agent",
                        Some(Message::SetStatus(self.selected.clone(), true)),
                    )),
                    "paused" => standing.push(action(
                        "agents/resume",
                        "Resume agent",
                        Some(Message::SetStatus(self.selected.clone(), false)),
                    )),
                    _ => {}
                }
            }
            items.push(section(
                "agents/standing",
                "Standing",
                [kit::spaced(
                    kit::centered_row("agents/standing-row", standing),
                    8.,
                )],
            ));
        }
        let mut identity = Vec::new();
        if self.creating {
            identity.push(kit::field(
                "agents/id-field",
                "Agent id",
                field(
                    "agents/id",
                    "a-dns-label, e.g. chiefduck",
                    &self.draft_id,
                    Message::BindDraftId,
                ),
            ));
            let invalid_id = !self.draft_id.is_empty() && !host::valid_agent_id(&self.draft_id);
            if invalid_id {
                identity.push(kit::wrapping(kit::tone_text(
                    "agents/id-error",
                    "An agent id is a lowercase DNS label: a-z, 0-9 and hyphens, no hyphen at either end.",
                    Tone::Danger,
                )));
            }
        } else {
            identity.push(kit::kv(
                "agents/id-row",
                "Agent id",
                kit::wrapping(kit::mono("agents/id", &self.draft_id)),
            ));
        }
        identity.push(kit::field(
            "agents/name-field",
            "Display name",
            field(
                "agents/name",
                "Display name",
                &self.draft_name,
                Message::BindDraftName,
            ),
        ));
        items.push(section("agents/identity", "Identity", identity));
        let capability = host::or_empty(&self.draft_capability);
        let executor = if self.can_edit {
            let options = host::capability_options(&self.capabilities, &capability);
            let choices = options.clone();
            Node::PickList {
                key: "AgentsView/root/editor/agent-capability".into(),
                selected: options
                    .iter()
                    .position(|value| value == &capability)
                    .map(|index| index as u32),
                options,
                placeholder: Some("Pick an executor…".into()),
                on_select: slots::handler(Box::new(move |index: u32| {
                    choices
                        .get(index as usize)
                        .cloned()
                        .map(Message::PickCapabilityOption)
                })),
                width: Some(Length::Fill),
                style: Default::default(),
                settings: Default::default(),
            }
        } else if capability.is_empty() {
            kit::secondary("agents/capability", "No executor chosen")
        } else {
            kit::badge("agents/capability", &capability, Tone::Neutral)
        };
        items.push(section("agents/executor", "Executor", [executor]));
        let mut skills = Vec::new();
        for skill in &self.draft_skills {
            let key = format!("agents/skill/{}", skill.name);
            let mut head = vec![kit::sized(
                kit::strong(format!("{key}/name"), &skill.name),
                Some(Length::Fill),
                None,
            )];
            if self.can_edit {
                head.push(subtle(
                    format!("{key}/load"),
                    if skill.always {
                        "Load on demand"
                    } else {
                        "Load always"
                    },
                    Some(Message::LoadSkill(skill.name.clone(), !skill.always)),
                ));
                head.push(subtle(
                    format!("{key}/remove"),
                    "Remove skill",
                    Some(Message::RemoveSkill(skill.name.clone())),
                ));
            } else {
                head.push(kit::badge(
                    format!("{key}/mode"),
                    sentence(&host::skill_mode(skill.always)),
                    Tone::Neutral,
                ));
            }
            let mut details = vec![kit::spaced(
                kit::centered_row(format!("{key}/head"), head),
                4.,
            )];
            if !skill.source_prefix.is_empty() {
                details.push(kit::wrapping(kit::mono(
                    format!("{key}/source"),
                    &skill.source_prefix,
                )));
            }
            if !skill.source_snapshot.is_empty() {
                details.push(kit::wrapping(kit::caption(
                    format!("{key}/snapshot"),
                    format!("Pinned to snapshot {}", skill.source_snapshot),
                )));
            }
            skills.push(kit::card(
                key.clone(),
                kit::spaced(kit::column(format!("{key}/body"), details), 4.),
            ));
        }
        if self.can_edit {
            skills.push(kit::card(
                "agents/add-skill-card",
                kit::spaced(
                    kit::column(
                        "agents/add-skill-body",
                        [
                            kit::label("agents/add-skill-title", "Add a skill"),
                            field(
                                "agents/skill-name",
                                "Skill name (its mount directory)",
                                &self.skill_name,
                                Message::BindSkillName,
                            ),
                            field(
                                "agents/skill-prefix",
                                "Source path, /shared/skills/<name> when empty",
                                &self.skill_prefix,
                                Message::BindSkillPrefix,
                            ),
                            field(
                                "agents/skill-snapshot",
                                "Snapshot to pin (optional)",
                                &self.skill_snapshot,
                                Message::BindSkillSnapshot,
                            ),
                            kit::centered_row(
                                "agents/add-skill-row",
                                [
                                    Node::Toggle {
                                        key: "agents/skill-always".into(),
                                        kind: wire::ToggleKind::Checkbox,
                                        label: "Load always (persona)".into(),
                                        checked: self.skill_always,
                                        on_toggle: Some(slots::handler(Box::new(|value| {
                                            Some(Message::SetSkillAlways(value))
                                        }))),
                                        style: Default::default(),
                                        width: Some(Length::Fill),
                                    },
                                    kit::spacer(),
                                    action(
                                        "agents/add-skill",
                                        "Add skill",
                                        (!self.skill_name.trim().is_empty())
                                            .then_some(Message::AddSkill),
                                    ),
                                ],
                            ),
                        ],
                    ),
                    8.,
                ),
            ));
        }
        items.push(section("agents/skills", "Skills", skills));
        let complete_record = !self.draft_name.trim().is_empty() && !capability.is_empty();
        let submit = match (self.creating, self.can_edit) {
            (true, _) => Some(primary(
                "agents/register",
                "Register agent",
                (complete_record
                    && host::valid_agent_id(&self.draft_id)
                    && self.registration.is_none())
                .then_some(Message::SubmitRegister),
            )),
            (false, true) => Some(primary(
                "agents/save",
                "Save agent",
                complete_record.then_some(Message::SubmitSave),
            )),
            (false, false) => None,
        };
        if let Some(submit) = submit {
            items.push(kit::centered_row("agents/submit-row", [filler(), submit]));
        }
        kit::spaced(
            kit::padded(
                kit::column("agents/editor-content", items),
                wire::Edges::all(16.),
            ),
            16.,
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RunTab {
    Conversation,
    Trace,
    Journal,
    Raw,
}
impl RunTab {
    const ALL: [Self; 4] = [Self::Conversation, Self::Trace, Self::Journal, Self::Raw];
    fn label(self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::Trace => "Trace",
            Self::Journal => "Journal",
            Self::Raw => "Raw",
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AgentsView {
    pub(crate) rows: Vec<crate::host::AgentRow>,
    pub(crate) runs: Vec<crate::host::RunRow>,
    pub(crate) journal: crate::host::RunJournal,
    pub(crate) live: crate::host::LiveRun,
    pub(crate) panel: String,
    pub(crate) open_run: String,
    pub(crate) run_list_width: f64,
    pub(crate) editor_width: f64,
    pub(crate) viewport_width: f64,
    pub(crate) expanded_receipt: String,
    pub(crate) trace_open: bool,
    pub(crate) run_tab: RunTab,
    pub(crate) raw_event_open: Option<String>,
    pub(crate) message_preview_open: Option<String>,
    pub(crate) control_draft: String,
    pub(crate) control_state: host::ControlState,
    pub(crate) control_serial: u64,
    pub(crate) open_row: crate::host::RunRow,
    pub(crate) opened: i64,
    pub(crate) capabilities: Vec<String>,
    pub(crate) account: String,
    pub(crate) committed: i64,
    pub(crate) seeded: i64,
    #[serde(skip)]
    registration: Option<ducktape_view_guest::task::Handle>,
    #[serde(skip)]
    registration_serial: u64,
    pub(crate) connected: bool,
    pub(crate) connection_serial: i64,
    pub(crate) answered: bool,
    pub(crate) host_error: String,
    pub(crate) selected: String,
    pub(crate) creating: bool,
    pub(crate) can_edit: bool,
    pub(crate) selected_status: String,
    pub(crate) draft_id: String,
    pub(crate) draft_name: String,
    pub(crate) draft_capability: Option<String>,
    pub(crate) draft_skills: Vec<crate::host::AgentSkill>,
    pub(crate) skill_name: String,
    pub(crate) skill_prefix: String,
    pub(crate) skill_snapshot: String,
    pub(crate) skill_always: bool,
    pub(crate) sent: bool,
    pub(crate) dark: bool,
}
impl ::std::fmt::Debug for AgentsView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("AgentsView")
    }
}
#[derive(Clone)]
pub enum Message {
    RunListResized(f64, f64),
    EditorResized(f64, f64),
    ViewportChanged(f64, f64),
    ToggleReceipt(String),
    ToggleTrace,
    ChooseRunTab(RunTab),
    ToggleRawEvent(String),
    ToggleMessagePreview(String),
    RetryTrace,
    ControlDraft(String),
    ControlSend,
    ControlInterrupt,
    ControlApprove(String, bool),
    ControlDone(String, u64, serde_json::Value, Result<(), String>),
    SessionArrived(crate::host::SessionItem),
    RegisterArrived(crate::host::RegisterItem),
    JournalArrived(crate::host::JournalItem),
    LiveArrived(crate::host::LiveRun),
    ActDone(crate::host::ActItem),
    OpenAgent(String),
    OpenNew,
    CloseEditor,
    ChoosePanel(String),
    OpenRunRow(String),
    CloseRun,
    OpenPlace(String),
    PickCapabilityOption(String),
    SetSkillAlways(bool),
    AddSkill,
    RemoveSkill(String),
    LoadSkill(String, bool),
    SetStatus(String, bool),
    SubmitSave,
    SubmitRegister,
    RegistrationDone(u64, Result<(), String>),
    BindDraftId(String),
    BindDraftName(String),
    BindSkillName(String),
    BindSkillPrefix(String),
    BindSkillSnapshot(String),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl AgentsView {
    fn state() -> Self {
        Self {
            rows: Vec::new(),
            runs: Vec::new(),
            journal: crate::host::empty_journal(),
            live: crate::host::empty_live(),
            panel: "registry".to_owned(),
            open_run: "".to_owned(),
            run_list_width: 260.0,
            editor_width: 400.0,
            viewport_width: 1280.0,
            expanded_receipt: "".to_owned(),
            trace_open: false,
            run_tab: RunTab::Conversation,
            raw_event_open: None,
            message_preview_open: None,
            control_draft: String::new(),
            control_state: host::ControlState::Idle,
            control_serial: 0,
            open_row: crate::host::empty_run(),
            opened: 0,
            capabilities: Vec::new(),
            account: "".to_owned(),
            committed: 0,
            seeded: 0,
            registration: None,
            registration_serial: 0,
            connected: false,
            connection_serial: 0,
            answered: false,
            host_error: "".to_owned(),
            selected: "".to_owned(),
            creating: false,
            can_edit: false,
            selected_status: "".to_owned(),
            draft_id: "".to_owned(),
            draft_name: "".to_owned(),
            draft_capability: None,
            draft_skills: Vec::new(),
            skill_name: "".to_owned(),
            skill_prefix: "".to_owned(),
            skill_snapshot: "".to_owned(),
            skill_always: false,
            sent: false,
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    /// This state's layout, digested — `snapshot_schema` holds it here.
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "b4af6857860fc1fedcbf5402adbf1fa3df726117feef73754437cd57fc4e005c";
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.validate_snapshot()?;
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid Agents snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Agents snapshot".into());
        };
        let mut state: Self = wire::decode(&state)?;
        if matches!(state.control_state, host::ControlState::Sending) {
            state.control_state = host::ControlState::Idle;
            state.control_serial = state.control_serial.wrapping_add(1);
        }
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let widths = [self.run_list_width, self.editor_width, self.viewport_width];
        if widths.into_iter().all(f64::is_finite) {
            Ok(())
        } else {
            Err("invalid Agents snapshot geometry".into())
        }
    }
}
impl AgentsView {
    fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        let run_open = self.connected && !self.open_run.is_empty();
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                host::register(self.connection_serial).map(Message::RegisterArrived)
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if run_open {
                host::run_journal(self.open_run.to_owned(), self.connection_serial)
                    .map(Message::JournalArrived)
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if run_open {
                host::live_run(self.open_run.to_owned(), self.connection_serial)
                    .map(Message::LiveArrived)
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            crate::host::acts().map(Message::ActDone),
        ])
    }
}
#[cfg(test)]
mod tests {

    #[test]
    fn local_run_navigation_survives_session_refresh_until_another_external_request() {
        let mut view = super::AgentsView::state();
        let session = |opened, run: &str| {
            super::Message::SessionArrived(crate::host::SessionItem {
                next: crate::host::Session {
                    connected: true,
                    open_run: run.into(),
                    opened,
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let _ = view.update(session(1, "external"));
        assert_eq!(view.open_run, "external");
        let _ = view.update(super::Message::CloseRun);
        let _ = view.update(session(1, "external"));
        assert!(
            view.open_run.is_empty(),
            "ordinary props cannot reopen a closed run"
        );
        view.open_run = "local".into();
        let _ = view.update(session(1, "external"));
        assert_eq!(view.open_run, "local");
        let _ = view.update(session(2, "external"));
        assert_eq!(
            view.open_run, "external",
            "a fresh external request is navigation"
        );
    }
    use super::*;

    #[test]
    fn registration_completion_preserves_a_different_open_draft() {
        for creating in [false, true] {
            let (mut view, _) = AgentsView::boot();
            view.registration_serial = 4;
            view.connected = true;
            view.account = "7".into();
            view.selected = "other-agent".into();
            view.draft_id = "other-agent".into();
            view.draft_name = "Unsaved name".into();
            view.creating = creating;
            let _ = view.on_registration_done(4, Ok(()));
            let mut rows = vec![host::AgentRow {
                id: "registered-agent".into(),
                ..Default::default()
            }];
            if !creating {
                rows.push(host::AgentRow {
                    id: "other-agent".into(),
                    name: "Stored name".into(),
                    ..Default::default()
                });
            }
            let _ = view.on_register_arrived(host::RegisterItem {
                rows,
                ..Default::default()
            });
            assert_eq!(view.draft_name, "Unsaved name");
            assert_eq!(view.creating, creating);
        }
    }

    #[test]
    fn long_reply_links_wrap_within_the_journal_width() {
        let label =
            "#Engineering · ChiefDuck: ".to_owned() + &"긴 답변 text without clipping ".repeat(80);
        let mut node = places(
            "places",
            &[host::RunLink {
                label: label.clone(),
                url: "duck://chat/general/4".into(),
                relation: "reply".into(),
                kind: "chat".into(),
                preview: None,
            }],
            None,
        );
        let mut labels = 0;
        node.for_each_mut(&mut |node| {
            if let Node::Text {
                content,
                options,
                width,
                ..
            } = node
                && content == &label
            {
                labels += 1;
                assert_eq!(options.wrapping, Some(wire::Wrapping::WordOrGlyph));
                assert_eq!(*width, Some(Length::Fill));
            }
        });
        assert_eq!(labels, 1);
    }

    #[test]
    fn callbacks_from_a_previous_turn_do_not_change_the_current_composer() {
        let (mut view, _) = AgentsView::boot();
        view.open_run = "run".into();
        view.live.control = Some(host::RunControl {
            turn: "new-turn".into(),
            steers: true,
            approvals: vec![],
        });
        view.control_draft = "new draft".into();
        view.control_state = host::ControlState::Sending;
        let _ = view.on_control_done(
            "run".into(),
            9,
            serde_json::json!({"action":"steer","text":"new draft"}),
            Ok(()),
        );
        assert_eq!(view.control_draft, "new draft");
        assert!(matches!(view.control_state, host::ControlState::Sending));
    }

    #[test]
    fn snapshot_refuses_foreign_schema_corrupt_payload_and_nonfinite_geometry() {
        let (mut state, _) = AgentsView::boot();
        let mut envelope = wire::Snapshot::decode(&state.snapshot().unwrap()).unwrap();
        envelope.schema = "0".repeat(64);
        assert!(AgentsView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = AgentsView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(AgentsView::restore(&envelope.encode().unwrap()).is_err());
        state.run_list_width = f64::INFINITY;
        assert!(state.snapshot().is_err());
        envelope.state = wire::SnapshotValue::Bytes(wire::encode(&state));
        assert!(AgentsView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn disconnected_view_hides_retained_registry_editor_and_runs() {
        let (mut view, _) = AgentsView::boot();
        view.rows.push(host::AgentRow {
            id: "stale-agent".into(),
            ..Default::default()
        });
        view.selected = "stale-agent".into();
        view.creating = true;
        view.can_edit = true;
        view.account = "7".into();
        view.open_run = "stale-run".into();
        for pane in ["registry", "runs"] {
            view.panel = pane.into();
            let mut tree = view.view();
            tree.for_each_mut(&mut |node| {
                assert!(!node.key().is_some_and(|key| key.contains("stale-agent")
                    || key.ends_with("/editor")
                    || key.ends_with("/journal")));
                assert!(!matches!(
                    node,
                    Node::Button {
                        on_press: Some(_),
                        ..
                    }
                ));
            });
        }
        assert_eq!(view.rows.len(), 1);
    }
    #[test]
    fn view_fits_default_stack() {
        let (app, _) = AgentsView::boot();
        let _ = app.view();
    }

    #[test]
    fn snapshot_preserves_editor_and_journal_state() {
        let (mut app, _) = AgentsView::boot();
        app.selected = "reviewer".into();
        app.draft_name = "Reviewer 한글".into();
        app.draft_capability = Some("review".into());
        app.draft_skills = vec![host::AgentSkill {
            name: "review".into(),
            source_prefix: "/shared/skills/review".into(),
            source_snapshot: "pinned".into(),
            always: true,
        }];
        app.rows.push(host::AgentRow {
            id: "reviewer".into(),
            name: "Reviewer 한글".into(),
            capability: "review".into(),
            skills: app.draft_skills.clone(),
            ..Default::default()
        });
        app.open_run = "dispatch".into();
        app.expanded_receipt = "dispatch".into();
        app.run_list_width = 480.;
        app.editor_width = 470.;
        let bytes = app.snapshot().unwrap();
        let restored = AgentsView::restore(&bytes).unwrap();
        assert_eq!(restored.snapshot().unwrap(), bytes);
    }
}
impl AgentsView {
    pub(crate) fn update(&mut self, message: Message) -> ::ducktape_view_guest::Task<Message> {
        match message {
            Message::RunListResized(dx, _dy) => self.on_run_list_resized(dx, _dy),
            Message::EditorResized(dx, _dy) => self.on_editor_resized(dx, _dy),
            Message::ViewportChanged(width, _height) => self.on_viewport_changed(width, _height),
            Message::ToggleReceipt(value) => self.on_toggle_receipt(value),
            Message::ToggleTrace => self.on_toggle_trace(),
            Message::ChooseRunTab(tab) => self.on_choose_run_tab(tab),
            Message::ToggleRawEvent(raw) => self.on_toggle_raw_event(raw),
            Message::ToggleMessagePreview(key) => self.on_toggle_message_preview(key),
            Message::RetryTrace => self.on_retry_trace(),
            Message::ControlDraft(text) => self.on_control_draft(text),
            Message::ControlSend => self.on_control_send(),
            Message::ControlInterrupt => self.on_control_interrupt(),
            Message::ControlApprove(id, allow) => self.on_control_approve(id, allow),
            Message::ControlDone(run, serial, input, result) => {
                self.on_control_done(run, serial, input, result)
            }
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::RegisterArrived(item) => self.on_register_arrived(item),
            Message::JournalArrived(item) => self.on_journal_arrived(item),
            Message::LiveArrived(item) => self.on_live_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::OpenAgent(id) => self.on_open_agent(id),
            Message::OpenNew => self.on_open_new(),
            Message::CloseEditor => self.on_close_editor(),
            Message::ChoosePanel(next) => self.on_choose_panel(next),
            Message::OpenRunRow(run_id) => self.on_open_run_row(run_id),
            Message::CloseRun => self.on_close_run(),
            Message::OpenPlace(url) => self.on_open_place(url),
            Message::PickCapabilityOption(value) => self.on_pick_capability_option(value),
            Message::SetSkillAlways(on) => self.on_set_skill_always(on),
            Message::AddSkill => self.on_add_skill(),
            Message::RemoveSkill(name) => self.on_remove_skill(name),
            Message::LoadSkill(name, always) => self.on_load_skill(name, always),
            Message::SetStatus(agent_id, paused) => self.on_set_status(agent_id, paused),
            Message::SubmitSave => self.on_submit_save(),
            Message::SubmitRegister => self.on_submit_register(),
            Message::RegistrationDone(serial, result) => self.on_registration_done(serial, result),
            Message::BindDraftId(value) => self.on_bind_draft_id(value),
            Message::BindDraftName(value) => self.on_bind_draft_name(value),
            Message::BindSkillName(value) => self.on_bind_skill_name(value),
            Message::BindSkillPrefix(value) => self.on_bind_skill_prefix(value),
            Message::BindSkillSnapshot(value) => self.on_bind_skill_snapshot(value),
        }
    }
    fn on_run_list_resized(&mut self, dx: f64, _dy: f64) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.run_list_width = crate::host::run_list_width_after_delta(
                    self.run_list_width,
                    dx,
                    self.viewport_width,
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_editor_resized(&mut self, dx: f64, _dy: f64) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.editor_width = crate::host::editor_width_after_delta(
                    self.editor_width,
                    -dx,
                    self.viewport_width,
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_viewport_changed(
        &mut self,
        width: f64,
        _height: f64,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.viewport_width = width;
            }
            {
                self.run_list_width =
                    crate::host::run_list_width_after_delta(self.run_list_width, 0.0, width);
            }
            {
                self.editor_width =
                    crate::host::editor_width_after_delta(self.editor_width, 0.0, width);
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_control_draft(&mut self, text: String) -> ducktape_view_guest::Task<Message> {
        self.control_draft = text;
        ducktape_view_guest::Task::none()
    }
    fn control_request(
        &mut self,
        action: &str,
        payload: serde_json::Value,
    ) -> ducktape_view_guest::Task<Message> {
        let Some(control) = &self.live.control else {
            return ducktape_view_guest::Task::none();
        };
        let allowed = !matches!(self.control_state, host::ControlState::Sending)
            && self.open_row.state == "running";
        if !allowed {
            return ducktape_view_guest::Task::none();
        }
        let mut input = payload;
        input["action"] = action.into();
        input["expected_turn"] = control.turn.clone().into();
        self.control_state = host::ControlState::Sending;
        let run = self.open_run.clone();
        self.control_serial = self.control_serial.wrapping_add(1);
        let serial = self.control_serial;
        ducktape_view_guest::Task::future(async move {
            let result = host::control_run(run.clone(), input.clone()).await;
            Message::ControlDone(run, serial, input, result)
        })
    }
    fn on_control_send(&mut self) -> ducktape_view_guest::Task<Message> {
        let allowed = self
            .live
            .control
            .as_ref()
            .is_some_and(|control| control.steers)
            && !self.control_draft.trim().is_empty()
            && self.control_draft.len() <= 8192;
        if !allowed {
            return ducktape_view_guest::Task::none();
        }
        self.control_request("steer", serde_json::json!({"text":self.control_draft}))
    }
    fn on_control_interrupt(&mut self) -> ducktape_view_guest::Task<Message> {
        self.control_request("interrupt", serde_json::json!({}))
    }
    fn on_control_approve(
        &mut self,
        id: String,
        allow: bool,
    ) -> ducktape_view_guest::Task<Message> {
        self.control_request(
            "approve",
            serde_json::json!({"request_id":id,"allow":allow}),
        )
    }
    fn on_control_done(
        &mut self,
        run: String,
        serial: u64,
        input: serde_json::Value,
        result: Result<(), String>,
    ) -> ducktape_view_guest::Task<Message> {
        let current = run == self.open_run && serial == self.control_serial;
        if !current {
            return ducktape_view_guest::Task::none();
        }
        self.control_state = match result {
            Ok(()) => {
                let sent_draft = input["action"] == "steer"
                    && input["text"].as_str() == Some(self.control_draft.as_str());
                if sent_draft {
                    self.control_draft.clear();
                }
                host::ControlState::Accepted
            }
            Err(error) => host::ControlState::Failed(error),
        };
        ducktape_view_guest::Task::none()
    }

    fn on_retry_trace(&mut self) -> ducktape_view_guest::Task<Message> {
        self.live = host::LiveRun::default();
        self.connection_serial = self.connection_serial.wrapping_add(1);
        ducktape_view_guest::Task::none()
    }

    fn on_choose_run_tab(&mut self, tab: RunTab) -> ducktape_view_guest::Task<Message> {
        self.run_tab = tab;
        if tab == RunTab::Trace {
            self.trace_open = true;
        }
        ducktape_view_guest::Task::none()
    }
    fn on_toggle_message_preview(&mut self, key: String) -> ducktape_view_guest::Task<Message> {
        self.message_preview_open = if self.message_preview_open.as_ref() == Some(&key) {
            None
        } else {
            Some(key)
        };
        ducktape_view_guest::Task::none()
    }
    fn on_toggle_raw_event(&mut self, raw: String) -> ducktape_view_guest::Task<Message> {
        self.raw_event_open = if self.raw_event_open.as_ref() == Some(&raw) {
            None
        } else {
            Some(raw)
        };
        ducktape_view_guest::Task::none()
    }

    fn on_toggle_trace(&mut self) -> ducktape_view_guest::Task<Message> {
        self.trace_open = !self.trace_open;
        ducktape_view_guest::Task::none()
    }

    fn on_toggle_receipt(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.expanded_receipt = crate::host::pick_str(
                    self.expanded_receipt != value,
                    ::std::convert::AsRef::as_ref(&(value)),
                    ::std::convert::AsRef::as_ref(&("")),
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_session_arrived(
        &mut self,
        item: crate::host::SessionItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.host_error = crate::host::fault("Could not read the session", &item.error);
            }
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            let next = item.next.clone();
            let navigation_changed = next.opened != self.opened;
            let connection_changed = next.connected != self.connected;
            let registration_context_changed = !next.connected || self.account != next.account;
            if registration_context_changed {
                self.registration.take();
                self.registration_serial = self.registration_serial.wrapping_add(1);
            }
            {
                self.connection_serial = crate::host::connection_serial_after(
                    self.connected,
                    next.connected,
                    self.connection_serial,
                );
            }
            {
                self.connected = next.connected;
            }
            {
                self.dark = next.dark;
            }
            {
                self.account = next.account.to_owned();
            }
            {
                let accept_navigation = navigation_changed || connection_changed;
                if accept_navigation {
                    self.control_state = host::ControlState::Idle;
                    self.control_serial = self.control_serial.wrapping_add(1);
                    self.control_draft.clear();
                    self.live = host::LiveRun::default();
                    self.trace_open = false;
                    self.raw_event_open = None;
                    self.message_preview_open = None;
                    self.run_tab = RunTab::Conversation;
                    self.open_run = match next.connected {
                        true => next.open_run.clone(),
                        false => String::new(),
                    };
                }
            }
            {
                self.open_row = crate::host::run_at(
                    ::std::convert::AsRef::as_ref(&(self.runs)),
                    ::std::convert::AsRef::as_ref(&(self.open_run)),
                );
            }
            let door_pressed = (next.opened != self.opened) && (!(next.open_run).is_empty());
            {
                self.opened = next.opened;
            }
            {
                self.panel = crate::host::pick_str(
                    door_pressed,
                    ::std::convert::AsRef::as_ref(&("runs")),
                    ::std::convert::AsRef::as_ref(&(self.panel)),
                );
            }
            let row = crate::host::row_named(
                ::std::convert::AsRef::as_ref(&(self.rows)),
                ::std::convert::AsRef::as_ref(&(self.selected)),
            );
            {
                self.can_edit = crate::host::editable(
                    self.connected,
                    ::std::convert::AsRef::as_ref(&(self.account)),
                    ::std::convert::AsRef::as_ref(&(row.controller)),
                );
            }
            {}
            {}
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_register_arrived(
        &mut self,
        item: crate::host::RegisterItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.host_error = crate::host::fault("Could not read the registry", &item.error);
            }
            {
                self.answered = true;
            }
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            {
                self.rows = item.rows.clone();
            }
            {
                self.runs = item.runs.clone();
            }
            {
                self.capabilities = item.capabilities.clone();
            }
            {
                self.open_row = crate::host::run_at(
                    ::std::convert::AsRef::as_ref(&(self.runs)),
                    ::std::convert::AsRef::as_ref(&(self.open_run)),
                );
            }
            {
                self.sent = crate::host::badge(crate::host::working_agents(
                    ::std::convert::AsRef::as_ref(&(self.rows)),
                ));
            }
            let consumed = crate::host::drafts_consumed(
                self.committed,
                self.seeded,
                self.creating,
                ::std::convert::AsRef::as_ref(&(self.rows)),
                ::std::convert::AsRef::as_ref(&(self.draft_id)),
            );
            {
                self.seeded = self.committed;
            }
            {
                self.selected = crate::host::pick_str(
                    consumed && self.creating,
                    ::std::convert::AsRef::as_ref(&(self.draft_id)),
                    ::std::convert::AsRef::as_ref(&(self.selected)),
                );
            }
            {
                self.creating = self.creating && (!consumed);
            }
            let row = crate::host::row_named(
                ::std::convert::AsRef::as_ref(&(self.rows)),
                ::std::convert::AsRef::as_ref(&(self.selected)),
            );
            {
                self.can_edit = crate::host::editable(
                    self.connected,
                    ::std::convert::AsRef::as_ref(&(self.account)),
                    ::std::convert::AsRef::as_ref(&(row.controller)),
                );
            }
            {
                self.selected_status = row.status.to_owned();
            }
            {
                self.draft_name = crate::host::pick_str(
                    consumed,
                    ::std::convert::AsRef::as_ref(&(row.name)),
                    ::std::convert::AsRef::as_ref(&(self.draft_name)),
                );
            }
            {
                self.draft_capability = crate::host::pick_capability(
                    consumed,
                    ::std::convert::AsRef::as_ref(&(row.capability)),
                    ::std::borrow::Borrow::borrow(&(self.draft_capability)),
                );
            }
            {
                self.draft_skills = crate::host::pick_skills(
                    consumed,
                    ::std::convert::AsRef::as_ref(&(row.skills)),
                    ::std::convert::AsRef::as_ref(&(self.draft_skills)),
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_journal_arrived(
        &mut self,
        item: crate::host::JournalItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.host_error =
                    crate::host::fault("Could not read this run's journal", &item.error);
            }
            if (!(item.error).is_empty()) || (item.journal.dispatch_id != self.open_run) {
                return ::ducktape_view_guest::Task::none();
            }
            {
                self.journal = item.journal.clone();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_live_arrived(
        &mut self,
        item: crate::host::LiveRun,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                let closed = self.live.control.is_some() && item.control.is_none();
                if closed {
                    self.control_state = host::ControlState::Idle;
                    self.control_serial = self.control_serial.wrapping_add(1);
                }
                self.live = item.clone();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.host_error = crate::host::fault("The change was not accepted", &item.error);
            }
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            {
                self.committed += 1;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_agent(&mut self, id: String) -> ::ducktape_view_guest::Task<Message> {
        {
            let row = crate::host::row_named(
                ::std::convert::AsRef::as_ref(&(self.rows)),
                ::std::convert::AsRef::as_ref(&(id)),
            );
            {
                self.selected = id.to_owned();
            }
            {
                self.creating = false;
            }
            {
                self.can_edit = crate::host::editable(
                    self.connected,
                    ::std::convert::AsRef::as_ref(&(self.account)),
                    ::std::convert::AsRef::as_ref(&(row.controller)),
                );
            }
            {
                self.selected_status = row.status.to_owned();
            }
            {
                self.draft_id = row.id.to_owned();
            }
            {
                self.draft_name = row.name.to_owned();
            }
            {
                self.draft_capability =
                    crate::host::some_str(::std::convert::AsRef::as_ref(&(row.capability)));
            }
            {
                self.draft_skills = row.skills.clone();
            }
            {
                self.skill_name = "".to_owned();
            }
            {
                self.skill_prefix = "".to_owned();
            }
            {
                self.skill_snapshot = "".to_owned();
            }
            {
                self.skill_always = false;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_new(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.panel = "registry".to_owned();
            }
            {
                self.selected = "".to_owned();
            }
            {
                self.creating = true;
            }
            {
                self.can_edit = self.connected && (!(self.account).is_empty());
            }
            {
                self.draft_id = "".to_owned();
            }
            {
                self.draft_name = "".to_owned();
            }
            {
                self.draft_capability = None;
            }
            {
                self.draft_skills = Vec::new();
            }
            {
                self.skill_name = "".to_owned();
            }
            {
                self.skill_prefix = "".to_owned();
            }
            {
                self.skill_snapshot = "".to_owned();
            }
            {
                self.skill_always = false;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_close_editor(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.selected = "".to_owned();
            }
            {
                self.creating = false;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_choose_panel(&mut self, next: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.panel = next.to_owned();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_run_row(&mut self, run_id: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.trace_open = false;
                self.raw_event_open = None;
                self.message_preview_open = None;
                self.run_tab = RunTab::Conversation;
                self.expanded_receipt = "".to_owned();
                self.control_state = host::ControlState::Idle;
                self.control_serial = self.control_serial.wrapping_add(1);
                self.control_draft.clear();
            }
            {
                self.open_row = crate::host::run_named(
                    ::std::convert::AsRef::as_ref(&(self.runs)),
                    ::std::convert::AsRef::as_ref(&(run_id)),
                );
            }
            {
                if self.open_run != self.open_row.dispatch_id {
                    self.live = host::LiveRun::default();
                }
                self.open_run = self.open_row.dispatch_id.to_owned();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_close_run(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.trace_open = false;
                self.raw_event_open = None;
                self.message_preview_open = None;
                self.run_tab = RunTab::Conversation;
                self.expanded_receipt = "".to_owned();
                self.control_state = host::ControlState::Idle;
                self.control_serial = self.control_serial.wrapping_add(1);
                self.control_draft.clear();
            }
            {
                self.open_run = "".to_owned();
            }
            {
                self.open_row = crate::host::empty_run();
            }
            {
                self.journal = crate::host::empty_journal();
            }
            {
                self.live = crate::host::empty_live();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_place(&mut self, url: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.sent = crate::host::open_link(::std::convert::AsRef::as_ref(&(url)));
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_pick_capability_option(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_capability = Some(value.to_owned());
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_set_skill_always(&mut self, on: bool) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.skill_always = on;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_add_skill(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_skills = crate::host::with_skill(
                    ::std::convert::AsRef::as_ref(&(self.draft_skills)),
                    ::std::convert::AsRef::as_ref(&(self.skill_name)),
                    ::std::convert::AsRef::as_ref(
                        &(crate::host::pick_str(
                            ((self.skill_prefix).trim().to_owned()).is_empty(),
                            ::std::convert::AsRef::as_ref(
                                &(crate::host::library_prefix(::std::convert::AsRef::as_ref(
                                    &(self.skill_name),
                                ))),
                            ),
                            ::std::convert::AsRef::as_ref(&(self.skill_prefix)),
                        )),
                    ),
                    ::std::convert::AsRef::as_ref(&(self.skill_snapshot)),
                    self.skill_always,
                );
            }
            {
                self.skill_name = "".to_owned();
            }
            {
                self.skill_prefix = "".to_owned();
            }
            {
                self.skill_snapshot = "".to_owned();
            }
            {
                self.skill_always = false;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_remove_skill(&mut self, name: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_skills = crate::host::without_skill(
                    ::std::convert::AsRef::as_ref(&(self.draft_skills)),
                    ::std::convert::AsRef::as_ref(&(name)),
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_load_skill(
        &mut self,
        name: String,
        always: bool,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_skills = crate::host::skill_loaded(
                    ::std::convert::AsRef::as_ref(&(self.draft_skills)),
                    ::std::convert::AsRef::as_ref(&(name)),
                    always,
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_set_status(
        &mut self,
        agent_id: String,
        paused: bool,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.sent = crate::host::status(::std::convert::AsRef::as_ref(&(agent_id)), paused);
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_submit_save(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.sent = crate::host::save(
                    ::std::convert::AsRef::as_ref(&(self.selected)),
                    ::std::convert::AsRef::as_ref(&(self.draft_name)),
                    ::std::convert::AsRef::as_ref(
                        &(crate::host::or_empty(::std::borrow::Borrow::borrow(
                            &(self.draft_capability),
                        ))),
                    ),
                    ::std::convert::AsRef::as_ref(&(self.draft_skills)),
                );
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_submit_register(&mut self) -> ducktape_view_guest::Task<Message> {
        let may_start = self.connected && self.registration.is_none();
        if !may_start {
            return ducktape_view_guest::Task::none();
        }
        let draft = host::Draft {
            agent_id: self.draft_id.trim().into(),
            display_name: self.draft_name.trim().into(),
            capability: self
                .draft_capability
                .clone()
                .unwrap_or_default()
                .trim()
                .into(),
            skills: self.draft_skills.clone(),
        };
        self.registration_serial = self.registration_serial.wrapping_add(1);
        let serial = self.registration_serial;
        let (task, handle) = ducktape_view_guest::Task::perform(
            host::register_agent(self.account.clone(), draft),
            move |result| Message::RegistrationDone(serial, result),
        )
        .abortable();
        self.registration = Some(handle.abort_on_drop());
        task
    }
    fn on_registration_done(
        &mut self,
        serial: u64,
        result: Result<(), String>,
    ) -> ducktape_view_guest::Task<Message> {
        if serial != self.registration_serial {
            return ducktape_view_guest::Task::none();
        }
        self.registration.take();
        // Registration consumes only the creation draft whose id appears in
        // the registry. A later callback must not consume another open editor.
        self.host_error = host::fault(
            "The change was not accepted",
            &result.err().unwrap_or_default(),
        );
        ducktape_view_guest::Task::none()
    }
    fn on_bind_draft_id(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_id = value;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_bind_draft_name(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.draft_name = value;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_bind_skill_name(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.skill_name = value;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_bind_skill_prefix(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.skill_prefix = value;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_bind_skill_snapshot(&mut self, value: String) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.skill_snapshot = value;
            }
            ::ducktape_view_guest::Task::none()
        }
    }
}
ducktape_view_guest::export_app!(
    AgentsView,
    "Agents",
    "The registry of who may act, on which executor, and what their runs did.",
    ["agents"]
);

#[cfg(test)]
mod snapshot_schema {
    use super::AgentsView;

    #[test]
    fn the_tag_is_this_state_s_layout() {
        view_wire::schema::holds::<AgentsView>(AgentsView::SNAPSHOT_SCHEMA, |_| {});
    }
}
