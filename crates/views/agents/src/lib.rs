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
    kit::button(
        key,
        label,
        message.map(slots::message),
        ButtonPreset::Subtle,
    )
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

fn places(key: &str, links: &[host::RunLink]) -> Node {
    kit::spaced(
        kit::wrapped_row(
            key,
            links.iter().enumerate().map(|(index, link)| {
                let key = format!("{key}/{index}");
                if link.url.is_empty() {
                    return kit::badge(key, &link.label, Tone::Neutral);
                }
                kit::button(
                    key,
                    &link.label,
                    Some(slots::message(Message::OpenPlace(link.url.clone()))),
                    ButtonPreset::Text,
                )
            }),
        ),
        6.,
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
/// amber running, grey otherwise.
fn state_tone(state: &str) -> Tone {
    let word = state.split(['·', ' ']).next().unwrap_or_default().trim();
    match word {
        "active" | "done" | "settled" | "merged" | "succeeded" => Tone::Success,
        "failed" | "error" | "refused" => Tone::Danger,
        "running" | "working" | "dispatched" | "pending" => Tone::Accent,
        _ => Tone::Neutral,
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
        if self.rows.is_empty() && self.answered && !self.creating {
            rows.push(kit::empty_state(
                "agents/empty",
                "No model agents configured",
                "Models appear here with their capability and skills once someone registers one.",
            ));
        }
        for agent in &self.rows {
            let key = format!("agents/record/{}", agent.id);
            let owner = if agent.owner_handle.is_empty() {
                "unowned"
            } else {
                &agent.owner_handle
            };
            let mut line = vec![
                kit::avatar(
                    format!("{key}/avatar"),
                    kit::initials(&agent.name),
                    Tone::Agent,
                ),
                kit::nowrap(kit::strong(format!("{key}/name"), &agent.name)),
                kit::nowrap(kit::caption(format!("{key}/owner"), owner)),
                kit::badge(
                    format!("{key}/capability"),
                    &agent.capability,
                    Tone::Neutral,
                ),
                filler(),
            ];
            if agent.live {
                line.push(kit::badge(
                    format!("{key}/working"),
                    "Working",
                    Tone::Accent,
                ));
            }
            line.push(kit::nowrap(kit::caption(
                format!("{key}/skills"),
                host::plural(agent.skills.len() as i64, "skill", "skills"),
            )));
            line.push(kit::badge(
                format!("{key}/standing"),
                &agent.status,
                state_tone(&agent.status),
            ));
            let summary = kit::spaced(kit::centered_row(format!("{key}/summary"), line), 8.);
            let mut button = kit::list_row(
                &key,
                summary,
                self.selected == agent.id,
                Some(slots::message(Message::OpenAgent(agent.id.clone()))),
            );
            if let Node::Button { label, .. } = &mut button {
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
        if self.runs.is_empty() {
            rows.push(kit::empty_state(
                "agents/no-runs",
                "No runs yet",
                "Every dispatch of an agent lands here with its journal.",
            ));
        }
        for run in &self.runs {
            let key = format!("agents/run/{}", run.run_id);
            let summary = kit::spaced(
                kit::centered_row(
                    format!("{key}/summary"),
                    [
                        kit::nowrap(kit::mono(format!("{key}/id"), &run.run_id)),
                        kit::badge(format!("{key}/state"), &run.state, state_tone(&run.state)),
                        kit::nowrap(kit::strong(format!("{key}/agent"), &run.agent_name)),
                        kit::nowrap(kit::secondary(format!("{key}/origin"), &run.origin)),
                        filler(),
                        kit::nowrap(kit::caption(format!("{key}/dispatched"), &run.dispatched)),
                    ],
                ),
                8.,
            );
            let mut button = kit::list_row(
                &key,
                summary,
                self.open_run == run.dispatch_id,
                Some(slots::message(Message::OpenRunRow(run.run_id.clone()))),
            );
            if let Node::Button { label, .. } = &mut button {
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
        let rail = (!self.open_run.is_empty()).then(|| {
            (
                resize("agents/journal-resize", Message::JournalResized),
                detail_pane(
                    "agents/journal",
                    kit::scroll("agents/journal-scroll", self.journal_panel()),
                    self.journal_width,
                ),
            )
        });
        self.split("agents/run-panes", list, rail)
    }

    fn journal_panel(&self) -> Node {
        let mut items = vec![
            kit::centered_row(
                "agents/journal-heading",
                [
                    kit::sized(
                        kit::wrapping(kit::heading("agents/journal-run", &self.open_row.run_id)),
                        Some(Length::Fill),
                        None,
                    ),
                    subtle(
                        "agents/close-journal",
                        "Close journal",
                        Some(Message::CloseRun),
                    ),
                ],
            ),
            kit::spaced(
                kit::centered_row(
                    "agents/journal-standing",
                    [
                        kit::badge(
                            "agents/journal-state",
                            &self.open_row.state,
                            state_tone(&self.open_row.state),
                        ),
                        subtle(
                            "agents/receipt",
                            "Run details",
                            Some(Message::ToggleReceipt(self.open_run.clone())),
                        ),
                    ],
                ),
                8.,
            ),
        ];
        if self.expanded_receipt == self.open_run {
            items.push(kit::card(
                "agents/receipt-card",
                kit::spaced(
                    kit::column(
                        "agents/receipt-body",
                        [
                            kit::kv(
                                "agents/dispatch",
                                "Dispatch",
                                kit::wrapping(kit::mono("agents/dispatch-id", &self.open_run)),
                            ),
                            kit::kv(
                                "agents/output",
                                "Output",
                                kit::wrapping(kit::mono(
                                    "agents/output-reference",
                                    &self.open_row.output_ref,
                                )),
                            ),
                        ],
                    ),
                    6.,
                ),
            ));
        }
        if self.live.present {
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
                                    Tone::Accent
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
        let journal_ready = self.journal.dispatch_id == self.open_run;
        if journal_ready && !self.journal.links.is_empty() {
            items.push(section(
                "agents/relevant",
                "Relevant",
                [places("agents/places", &self.journal.links)],
            ));
        }
        if !self.open_row.reason.is_empty() {
            items.push(kit::notice(
                "agents/run-reason-box",
                kit::wrapping(kit::text("agents/run-reason", &self.open_row.reason)),
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
                "This run's journal has no entries yet — the fold may still be catching up to the chain.",
            )));
        } else {
            for (index, entry) in self.journal.entries.iter().enumerate() {
                let key = format!("agents/entry/{index}");
                let body = kit::spaced(
                    kit::column(
                        format!("{key}/body"),
                        [
                            kit::spaced(
                                kit::centered_row(
                                    format!("{key}/head"),
                                    [
                                        kit::sized(
                                            kit::strong(format!("{key}/kind"), &entry.kind),
                                            Some(Length::Fill),
                                            None,
                                        ),
                                        kit::badge(
                                            format!("{key}/status"),
                                            &entry.status,
                                            state_tone(&entry.status),
                                        ),
                                    ],
                                ),
                                8.,
                            ),
                            kit::wrapping(kit::text(format!("{key}/summary"), &entry.summary)),
                            places(&format!("{key}/targets"), &entry.targets),
                        ],
                    ),
                    4.,
                );
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
        kit::spaced(
            kit::padded(
                kit::column("agents/journal-content", items),
                wire::Edges::all(16.),
            ),
            14.,
        )
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
            let mut standing = vec![kit::badge(
                "agents/status",
                &self.selected_status,
                state_tone(&self.selected_status),
            )];
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
                "display name…",
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
                placeholder: Some("pick a capability…".into()),
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
        } else {
            kit::badge("agents/capability", &capability, Tone::Agent)
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
                    host::skill_mode(skill.always),
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
                    &skill.source_snapshot,
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
                                "skill name (its mount directory)…",
                                &self.skill_name,
                                Message::BindSkillName,
                            ),
                            field(
                                "agents/skill-prefix",
                                "/shared/skills/<name> when left empty",
                                &self.skill_prefix,
                                Message::BindSkillPrefix,
                            ),
                            field(
                                "agents/skill-snapshot",
                                "snapshot id to pin (optional)",
                                &self.skill_snapshot,
                                Message::BindSkillSnapshot,
                            ),
                            kit::centered_row(
                                "agents/add-skill-row",
                                [
                                    Node::Toggle {
                                        key: "agents/skill-always".into(),
                                        kind: wire::ToggleKind::Checkbox,
                                        label: "load always (persona)".into(),
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
                (complete_record && host::valid_agent_id(&self.draft_id))
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AgentsView {
    pub(crate) rows: Vec<crate::host::AgentRow>,
    pub(crate) runs: Vec<crate::host::RunRow>,
    pub(crate) journal: crate::host::RunJournal,
    pub(crate) live: crate::host::LiveRun,
    pub(crate) panel: String,
    pub(crate) open_run: String,
    pub(crate) journal_width: f64,
    pub(crate) editor_width: f64,
    pub(crate) viewport_width: f64,
    pub(crate) expanded_receipt: String,
    pub(crate) open_row: crate::host::RunRow,
    pub(crate) opened: i64,
    pub(crate) capabilities: Vec<String>,
    pub(crate) account: String,
    pub(crate) committed: i64,
    pub(crate) seeded: i64,
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
    JournalResized(f64, f64),
    EditorResized(f64, f64),
    ViewportChanged(f64, f64),
    ToggleReceipt(String),
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
            journal_width: 400.0,
            editor_width: 400.0,
            viewport_width: 1280.0,
            expanded_receipt: "".to_owned(),
            open_row: crate::host::empty_run(),
            opened: 0,
            capabilities: Vec::new(),
            account: "".to_owned(),
            committed: 0,
            seeded: 0,
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
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "165a8c2b163b16b6eee611dd7fa68f512771c2fccdb283444299e7cad67bce23";
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
        let state: Self = wire::decode(&state)?;
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let widths = [self.journal_width, self.editor_width, self.viewport_width];
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
    use super::*;
    #[test]
    fn snapshot_refuses_foreign_schema_corrupt_payload_and_nonfinite_geometry() {
        let (mut state, _) = AgentsView::boot();
        let mut envelope = wire::Snapshot::decode(&state.snapshot().unwrap()).unwrap();
        envelope.schema = "0".repeat(64);
        assert!(AgentsView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = AgentsView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(AgentsView::restore(&envelope.encode().unwrap()).is_err());
        state.journal_width = f64::INFINITY;
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
        app.journal_width = 480.;
        app.editor_width = 470.;
        let bytes = app.snapshot().unwrap();
        let restored = AgentsView::restore(&bytes).unwrap();
        assert_eq!(restored.snapshot().unwrap(), bytes);
    }
}
impl AgentsView {
    pub(crate) fn update(&mut self, message: Message) -> ::ducktape_view_guest::Task<Message> {
        match message {
            Message::JournalResized(dx, _dy) => self.on_journal_resized(dx, _dy),
            Message::EditorResized(dx, _dy) => self.on_editor_resized(dx, _dy),
            Message::ViewportChanged(width, _height) => self.on_viewport_changed(width, _height),
            Message::ToggleReceipt(value) => self.on_toggle_receipt(value),
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
            Message::BindDraftId(value) => self.on_bind_draft_id(value),
            Message::BindDraftName(value) => self.on_bind_draft_name(value),
            Message::BindSkillName(value) => self.on_bind_skill_name(value),
            Message::BindSkillPrefix(value) => self.on_bind_skill_prefix(value),
            Message::BindSkillSnapshot(value) => self.on_bind_skill_snapshot(value),
        }
    }
    fn on_journal_resized(&mut self, dx: f64, _dy: f64) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.journal_width = crate::host::journal_width_after_delta(
                    self.journal_width,
                    -dx,
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
                self.journal_width =
                    crate::host::journal_width_after_delta(self.journal_width, 0.0, width);
            }
            {
                self.editor_width =
                    crate::host::editor_width_after_delta(self.editor_width, 0.0, width);
            }
            ::ducktape_view_guest::Task::none()
        }
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
                self.host_error = item.error.to_owned();
            }
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            let next = item.next.clone();
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
                self.open_run = next.open_run.to_owned();
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
                self.host_error = item.error.to_owned();
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
                self.host_error = item.error.to_owned();
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
                self.live = item.clone();
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.host_error = item.error.to_owned();
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
                self.expanded_receipt = "".to_owned();
            }
            {
                self.open_row = crate::host::run_named(
                    ::std::convert::AsRef::as_ref(&(self.runs)),
                    ::std::convert::AsRef::as_ref(&(run_id)),
                );
            }
            {
                self.open_run = self.open_row.dispatch_id.to_owned();
            }
            {
                self.sent = crate::host::open_run(::std::convert::AsRef::as_ref(
                    &(self.open_row.dispatch_id),
                ));
            }
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_close_run(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.expanded_receipt = "".to_owned();
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
            {
                self.sent = crate::host::open_run(::std::convert::AsRef::as_ref(&("")));
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
    fn on_submit_register(&mut self) -> ::ducktape_view_guest::Task<Message> {
        {
            {
                self.sent = crate::host::register_agent(
                    ::std::convert::AsRef::as_ref(&(self.draft_id)),
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
