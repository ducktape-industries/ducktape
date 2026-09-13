use super::kit::state_tone;
use super::*;
use crate::host;
use ducktape_view_guest::{kit::Tone, slots};

pub(super) fn action(key: impl Into<String>, label: &str, message: Option<Message>) -> wire::Node {
    native::button(
        key,
        label,
        message.map(slots::message),
        wire::ButtonPreset::Secondary,
    )
}

pub(super) fn subtle(key: impl Into<String>, label: &str, message: Option<Message>) -> wire::Node {
    native::button(
        key,
        label,
        message.map(slots::message),
        wire::ButtonPreset::Subtle,
    )
}

pub(super) fn primary(key: impl Into<String>, label: &str, message: Option<Message>) -> wire::Node {
    native::button(
        key,
        label,
        message.map(slots::message),
        wire::ButtonPreset::Primary,
    )
}

/// A titled block of an item screen.
pub(super) fn section(key: &str, title: wire::Node, children: Vec<wire::Node>) -> wire::Node {
    let mut items = vec![title];
    items.extend(children);
    native::spaced(native::column(key, items), 10.)
}

/// A caption over a value: one fact of the item's rail.
pub(super) fn fact(key: &str, name: &str, value: wire::Node) -> wire::Node {
    native::spaced(
        native::column(key, [native::caption(format!("{key}/label"), name), value]),
        3.,
    )
}

/// A 28px strip over a list: a quiet name at the left, a count at the right.
pub(super) fn strip(key: &str, name: &str, aside: Option<wire::Node>) -> wire::Node {
    let mut children = vec![native::sized(
        native::nowrap(native::label(format!("{key}/label"), name)),
        Some(wire::Length::Fill),
        None,
    )];
    children.extend(aside);
    native::padded(
        native::sized(
            native::spaced(native::centered_row(key, children), 6.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(28.)),
        ),
        wire::Edges {
            top: 0.,
            right: 12.,
            bottom: 0.,
            left: 12.,
        },
    )
}

fn picker(
    key: &str,
    options: Vec<String>,
    selected: &str,
    placeholder: Option<String>,
    route: fn(String) -> Message,
) -> wire::Node {
    let selected = options
        .iter()
        .position(|value| value == selected)
        .map(|index| index as u32);
    let choices = options.clone();
    wire::Node::PickList {
        key: key.into(),
        options,
        selected,
        placeholder,
        on_select: slots::handler(Box::new(move |index: u32| {
            choices.get(index as usize).cloned().map(route)
        })),
        width: None,
        style: Default::default(),
        settings: Default::default(),
    }
}

/// The page inset a reading wears; a split screen runs to the edges instead.
pub(super) const INSET: f32 = 20.;
/// The repository rail's width.
const RAIL: f32 = 240.;
/// An item's properties stand beside its reading from this viewport width;
/// narrower, they stack above it.
const TWO_COLUMN: f64 = 960.;
/// The item's properties column.
const PROPERTIES: f32 = 240.;

impl ForgeView {
    pub(super) fn forge_screen(&self) -> wire::Node {
        if !self.connected {
            return self.disconnected("forge/disconnected".into());
        }
        let mut split = native::sized(
            native::spaced(
                native::row(
                    "forge/split",
                    [
                        self.rail(),
                        native::vertical_divider("forge/rail-edge"),
                        self.repo_pane(),
                    ],
                ),
                0.,
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        );
        if let wire::Node::Linear { clip, .. } = &mut split {
            *clip = true;
        }
        split
    }

    /// Every repository on the network, one row each, the open one chosen.
    /// The rail is the namespace — no page introduces it.
    fn rail(&self) -> wire::Node {
        let p = native::palette();
        let count = native::nowrap(native::caption(
            "forge/repo-count",
            host::plural(self.repos.len() as i64, "repository", "repositories"),
        ));
        let mut content = vec![strip("forge/rail-head", "Repositories", Some(count))];
        let status = match self.list_phase.as_str() {
            "loading" => "Loading repositories…",
            "failed" => "Could not load repositories. Reconnect to retry.",
            _ => "",
        };
        let listed = !self.repos.is_empty();
        if !status.is_empty() && !listed {
            content.push(native::padded(
                native::row(
                    "forge/list-status-row",
                    [native::wrapping(native::secondary(
                        "forge/list-status",
                        status,
                    ))],
                ),
                wire::Edges {
                    top: 4.,
                    right: 12.,
                    bottom: 4.,
                    left: 12.,
                },
            ));
        }
        let rows = self.repos.iter().map(|repo| {
            let key = format!("forge/repo/{}", repo.name);
            let mut row = native::list_row(
                &key,
                native::centered_row(
                    format!("{key}/line"),
                    [
                        native::sized(
                            native::nowrap(native::strong(
                                format!("forge/open/{}", repo.name),
                                &repo.name,
                            )),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        native::nowrap(native::colored(
                            native::text_size(
                                native::mono(format!("forge/head/{}", repo.name), &repo.head),
                                native::type_scale::CAPTION as f32,
                            ),
                            p.faint,
                        )),
                    ],
                ),
                repo.name == self.open_repo,
                Some(slots::message(Message::ForgeOpenRepo(repo.name.clone()))),
            );
            if let wire::Node::Button { label, .. } = &mut row {
                *label = Some(repo.name.clone());
            }
            row
        });
        content.push(native::padded(
            native::spaced(native::column("forge/repo-rows", rows), 1.),
            wire::Edges {
                top: 0.,
                right: 6.,
                bottom: 6.,
                left: 6.,
            },
        ));
        native::pane(
            "forge/rail",
            native::scroll(
                "forge/rail-scroll",
                native::spaced(native::column("forge/rail-content", content), 2.),
            ),
            wire::Length::Fixed(RAIL),
        )
    }

    /// The right of the split: what the rail has open, or the way to a first
    /// repository when it holds none.
    fn repo_pane(&self) -> wire::Node {
        if self.open_repo.is_empty() {
            return self.welcome();
        }
        self.repo_screen()
    }

    /// No repository open: a refusal if one stands, then either the push
    /// command that makes the first repository, or the invitation to pick.
    fn welcome(&self) -> wire::Node {
        let mut content = Vec::new();
        if !self.host_error.is_empty() {
            content.push(self.unavailable("forge/error".into()));
        }
        let none_yet = self.repos.is_empty() && self.list_phase == "ready";
        if none_yet {
            content.push(native::empty_state(
                "forge/no-repos",
                "No repositories yet",
                "Push a git repository to this network to create one.",
            ));
            content.push(self.push_box());
        } else {
            content.push(native::empty_state(
                "forge/choose-repo",
                "No repository open",
                "Choose a repository from the list.",
            ));
        }
        native::scroll(
            "forge/welcome",
            native::padded(
                native::spaced(native::column("forge/welcome-content", content), 4.),
                wire::Edges::all(INSET),
            ),
        )
    }

    /// The command that makes the first repository: a code box, and the one
    /// control that puts it on the clipboard.
    fn push_box(&self) -> wire::Node {
        let p = native::palette();
        let command = host::forge_push_command(&self.connected_rpc);
        let mut node = native::container(
            "forge/push-card",
            native::centered_row(
                "forge/push-row",
                [
                    native::sized(
                        native::wrapping(native::mono("forge/push-command", &command)),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    action(
                        "forge/copy-push",
                        "Copy command",
                        Some(Message::CopyToClipboard(command, "Command copied".into())),
                    ),
                ],
            ),
        );
        if let wire::Node::Container {
            background,
            border,
            padding,
            max_width,
            ..
        } = &mut node
        {
            *background = Some(wire::Background::Color(native::rgba(p.surface)));
            *border = Some(wire::Border {
                color: Some(native::rgba(p.border)),
                width: Some(1.),
                radius: Some([native::radius::CONTROL as f32; 4]),
            });
            *padding = Some(wire::Edges {
                top: 6.,
                right: 6.,
                bottom: 6.,
                left: 10.,
            });
            *max_width = Some(720.);
        }
        node
    }

    /// One open repository: the header strip, its hairline, and whichever
    /// screen the tabs stand on.
    fn repo_screen(&self) -> wire::Node {
        let mut content = vec![self.toolbar(), native::divider("forge/toolbar-edge")];
        if !self.host_error.is_empty() {
            content.push(native::padded(
                native::row("forge/error-row", [self.unavailable("forge/error".into())]),
                wire::Edges {
                    top: 12.,
                    right: INSET,
                    bottom: 0.,
                    left: INSET,
                },
            ));
        }
        content.push(if self.forge_item_number > 0 {
            self.item_screen()
        } else {
            match self.tab.as_str() {
                "code" => self.code_screen(),
                "issues" => self.tracker_screen("issues"),
                _ => self.tracker_screen("pulls"),
            }
        });
        native::sized(
            native::spaced(native::column("forge/root", content), 0.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }

    /// The repository header: where you are on the left, as a crumb, and
    /// what you are reading on the right.
    fn toolbar(&self) -> wire::Node {
        let p = native::palette();
        let in_item = self.forge_item_number > 0;
        let mut header = vec![native::nowrap(native::weighted(
            native::text("forge/repo-name", &self.open_repo),
            wire::Weight::Semibold,
        ))];
        if in_item {
            let tracker = match self.tab.as_str() {
                "issues" => "Issues",
                _ => "Pull requests",
            };
            header.push(native::nowrap(native::colored(
                native::text("forge/crumb", "/"),
                p.faint,
            )));
            header.push(subtle(
                "forge/back",
                tracker,
                Some(Message::ForgeCloseItem),
            ));
            header.push(native::nowrap(native::colored(
                native::text("forge/crumb-item", "/"),
                p.faint,
            )));
            header.push(native::nowrap(native::colored(
                native::mono("forge/crumb-number", format!("#{}", self.forge_item_number)),
                p.muted,
            )));
        } else if !self.branches.is_empty() {
            header.push(picker(
                "ForgeView/forge/branch-pick",
                host::branch_names(&self.branches),
                &host::forge_tree_branch(&self.branches, &self.tree_pick, &self.tree_rev),
                Some(host::commit_label(&self.tree_rev)),
                Message::ForgePickBranch,
            ));
        }
        header.push(native::spacer());
        header.push(self.tab_row());
        native::sized(
            native::padded(
                native::spaced(native::centered_row("forge/navigation", header), 8.),
                wire::Edges {
                    top: 0.,
                    right: 8.,
                    bottom: 0.,
                    left: 16.,
                },
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(40.)),
        )
    }

    /// Code, then the two trackers, each wearing its open count when there
    /// is one.
    fn tab_row(&self) -> wire::Node {
        let choices = [
            ("code", "Code", ""),
            ("issues", "Issues", "issue"),
            ("pulls", "Pull requests", "pr"),
        ]
        .into_iter()
        .map(|(tab, label, kind)| {
            let open = host::forge_open_count(&self.items, kind);
            let counted = !kind.is_empty() && open > 0;
            let name = if counted {
                format!("{label} {open}")
            } else {
                label.to_owned()
            };
            (
                tab.to_owned(),
                name,
                self.tab == tab,
                Some(slots::message(Message::SelectForgeTab(tab.to_owned()))),
            )
        });
        native::sized(
            native::tabs("forge/tab", choices),
            Some(wire::Length::Shrink),
            None,
        )
    }

    /// The tracker: one dense line per item — its number, its state, its
    /// title, who opened it. The list is the screen: no card, no inset.
    fn tracker_screen(&self, tab: &str) -> wire::Node {
        let p = native::palette();
        let items = host::filter_forge_items(&self.items, tab);
        let mut content = Vec::new();
        match self.repo_phase.as_str() {
            "loading" => content.push(self.loading_tracker("forge/tracker-loading".into())),
            "failed" => content.push(self.tracker_unavailable("forge/tracker-failed".into())),
            "ready" => {
                if items.is_empty() {
                    content.push(if tab == "issues" {
                        self.empty_issues("forge/no-issues".into())
                    } else {
                        self.empty_pulls("forge/no-pulls".into())
                    });
                }
                for item in items {
                    let key = format!("forge/item/{}", item.number);
                    let line = native::sized(
                        native::spaced(
                            native::centered_row(
                                format!("{key}/line"),
                                [
                                    native::sized(
                                        native::nowrap(native::colored(
                                            native::mono(
                                                format!("{key}/number"),
                                                format!("#{}", item.number),
                                            ),
                                            p.faint,
                                        )),
                                        Some(wire::Length::Fixed(44.)),
                                        None,
                                    ),
                                    native::badge(
                                        format!("{key}/state"),
                                        host::state_label(&item.state),
                                        state_tone(&item.state),
                                    ),
                                    native::sized(
                                        native::nowrap(native::text(
                                            format!("{key}/open"),
                                            &item.title,
                                        )),
                                        Some(wire::Length::Fill),
                                        None,
                                    ),
                                    native::nowrap(native::caption(
                                        format!("{key}/author"),
                                        &item.author_name,
                                    )),
                                ],
                            ),
                            10.,
                        ),
                        Some(wire::Length::Fill),
                        Some(wire::Length::Fixed(24.)),
                    );
                    let mut row = native::list_row(
                        &key,
                        line,
                        false,
                        Some(slots::message(Message::ForgeOpenItem(item.number))),
                    );
                    if let wire::Node::Button { label, .. } = &mut row {
                        *label = Some(item.title.clone());
                    }
                    content.push(row);
                }
            }
            _ => {}
        }
        native::scroll(
            "forge/tracker",
            native::padded(
                native::spaced(native::column("forge/tracker-content", content), 1.),
                wire::Edges {
                    top: 6.,
                    right: 8.,
                    bottom: 12.,
                    left: 8.,
                },
            ),
        )
    }

    /// One item: its title band, then the reading beside its properties —
    /// or over them, when the viewport is too narrow for two columns.
    fn item_screen(&self) -> wire::Node {
        let mut content = Vec::new();
        match self.item_phase.as_str() {
            "loading" => content.push(self.loading_item("forge/item-loading".into())),
            "failed" => content.push(self.item_unavailable("forge/item-failed".into())),
            "ready" => {
                content.push(self.item_head());
                let mut reading = native::spaced(
                    native::column("forge/item-reading", self.item_reading()),
                    20.,
                );
                let properties = self.item_properties();
                if properties.is_empty() {
                    // alone, the reading keeps a measure a person can track
                    if let wire::Node::Linear { max_width, .. } = &mut reading {
                        *max_width = Some(760.);
                    }
                    content.push(reading);
                    return self.item_page(content);
                }
                let two_column = self.viewport_width >= TWO_COLUMN;
                let properties = native::spaced(
                    native::column("forge/item-properties", properties),
                    16.,
                );
                if two_column {
                    content.push(native::spaced(
                        native::row(
                            "forge/item-columns",
                            [
                                native::sized(reading, Some(wire::Length::Fill), None),
                                native::sized(
                                    properties,
                                    Some(wire::Length::Fixed(PROPERTIES)),
                                    None,
                                ),
                            ],
                        ),
                        32.,
                    ));
                } else {
                    content.push(properties);
                    content.push(reading);
                }
            }
            _ => {}
        }
        self.item_page(content)
    }

    /// The item's scrolling page: inset, and held to a width two columns
    /// still read across.
    fn item_page(&self, content: Vec<wire::Node>) -> wire::Node {
        let mut column = native::spaced(
            native::padded(
                native::column("forge/item-content", content),
                wire::Edges::all(INSET),
            ),
            20.,
        );
        if let wire::Node::Linear { max_width, .. } = &mut column {
            *max_width = Some(1180.);
        }
        native::scroll("forge/item-scroll", column)
    }

    /// The title, and under it the line that places the item: its state,
    /// its number, who opened it, and the link that names it.
    fn item_head(&self) -> wire::Node {
        let p = native::palette();
        let mut meta = vec![
            native::badge(
                "forge/item-state",
                host::state_label(&self.forge_item_state),
                state_tone(&self.forge_item_state),
            ),
            native::nowrap(native::colored(
                native::mono("forge/item-number", format!("#{}", self.forge_item_number)),
                p.muted,
            )),
            native::nowrap(native::secondary(
                "forge/item-author",
                &self.forge_item_author,
            )),
        ];
        meta.push(native::spacer());
        meta.push(subtle(
            "forge/copy-item",
            "Copy link",
            Some(Message::CopyToClipboard(
                host::duck_forge_item_link(
                    &self.open_repo,
                    self.forge_item_number,
                    &self.network_chain_id,
                ),
                "Link copied".into(),
            )),
        ));
        let title = native::wrapping(native::title("forge/item-title", &self.forge_item_title));
        native::spaced(
            native::column(
                "forge/item-head",
                [
                    title,
                    native::spaced(native::centered_row("forge/item-meta", meta), 8.),
                ],
            ),
            8.,
        )
    }

    /// The reading: the body, then for a pull request its changes and
    /// reviews, then the discussion every item carries.
    fn item_reading(&self) -> Vec<wire::Node> {
        let mut content = Vec::new();
        if !self.forge_item_body.is_empty() {
            content.push(self.item_body("forge/item-body".into(), Message::OpenMessageLink));
        }
        if self.forge_item_kind == "pr" {
            content.push(self.diff_screen());
            content.push(self.review_screen());
        }
        let mut discussion = Vec::new();
        for note in &self.linked_note {
            discussion.push(native::label(
                format!("forge/linked/{}/title", note.seq),
                "Linked note",
            ));
            discussion.push(self.note(format!("forge/linked/{}", note.seq), note));
        }
        if self.discussion.is_empty() && !self.discussion_clipped {
            discussion.push(native::secondary(
                "forge/no-discussion",
                "No discussion yet.",
            ));
        }
        if self.discussion_clipped {
            discussion.push(native::caption(
                "forge/discussion-clipped",
                "Older comments are not shown.",
            ));
        }
        for note in &self.discussion {
            discussion.push(self.note(format!("forge/note/{}", note.seq), note));
        }
        discussion.push(wire::Node::Surface {
            key: "forge/note-composer".into(),
            name: "forge_composer".into(),
            args: vec![
                wire::SurfaceValue::Str(host::composer_scope(
                    &self.connected_rpc,
                    &self.forge_item_channel,
                )),
                wire::SurfaceValue::Str("note".into()),
                wire::SurfaceValue::Bool(true),
                wire::SurfaceValue::Str("Write a note…".into()),
                wire::SurfaceValue::Bool(
                    !self.connected
                        || self.forge_item_channel.is_empty()
                        || self.item_phase != "ready",
                ),
                wire::SurfaceValue::Bool(false),
                wire::SurfaceValue::Str("The note wasn’t sent".into()),
            ],
            on_event: None,
        });
        content.push(section(
            "forge/discussion",
            native::heading("forge/discussion-title", "Discussion"),
            discussion,
        ));
        content
    }

    /// A pull request's properties beside its reading: the merge door, then
    /// what the title band does not say — its branches, its source commit,
    /// and who has reviewed it with what verdict. An issue has none: its
    /// state and author already stand under its title.
    fn item_properties(&self) -> Vec<wire::Node> {
        if self.forge_item_kind != "pr" {
            return Vec::new();
        }
        let p = native::palette();
        let mut facts = vec![self.merge_screen()];
        if !self.forge_item_branches.is_empty() {
            facts.push(fact(
                "forge/fact-branches",
                "Branches",
                native::wrapping(native::text(
                    "forge/fact-branches/value",
                    &self.forge_item_branches,
                )),
            ));
        }
        if !self.forge_item_source_oid.is_empty() {
            facts.push(fact(
                "forge/fact-head",
                "Source commit",
                native::nowrap(native::colored(
                    native::mono(
                        "forge/fact-head/value",
                        host::commit_label(&self.forge_item_source_oid),
                    ),
                    p.muted,
                )),
            ));
        }
        let reviewers: Vec<wire::Node> = self
            .forge_item_reviews
            .iter()
            .enumerate()
            .map(|(index, review)| {
                let key = format!("forge/reviewer/{index}");
                let mut line = vec![
                    native::sized(
                        native::nowrap(native::text(format!("{key}/name"), &review.author_name)),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    native::badge(
                        format!("{key}/verdict"),
                        host::verdict_label(&review.verdict),
                        state_tone(&review.verdict),
                    ),
                ];
                if review.outdated {
                    line.push(native::badge(
                        format!("{key}/outdated"),
                        "Outdated",
                        Tone::Warning,
                    ));
                }
                native::spaced(native::centered_row(key, line), 6.)
            })
            .collect();
        let reviewed = !reviewers.is_empty();
        facts.push(fact(
            "forge/fact-reviewers",
            "Reviewers",
            if reviewed {
                native::spaced(native::column("forge/reviewer-rows", reviewers), 4.)
            } else {
                native::secondary("forge/no-reviewers", "No reviews yet.")
            },
        ));
        facts
    }

    /// One note in the discussion: an avatar, then who wrote it and what it
    /// says. A row, not a card — the avatar column is the structure.
    fn note(&self, key: String, note: &host::ChatMessage) -> wire::Node {
        let human = note.avatar_kind == "human";
        native::spaced(
            native::row(
                &key,
                [
                    native::avatar(
                        format!("{key}/avatar"),
                        note.initial.clone(),
                        if human { Tone::Neutral } else { Tone::Agent },
                    ),
                    native::sized(
                        native::spaced(
                            native::column(
                                format!("{key}/body-column"),
                                [
                                    native::spaced(
                                        native::centered_row(
                                            format!("{key}/head"),
                                            [
                                                native::nowrap(native::strong(
                                                    format!("{key}/author"),
                                                    &note.author,
                                                )),
                                                native::nowrap(native::caption(
                                                    format!("{key}/meta"),
                                                    &note.meta,
                                                )),
                                            ],
                                        ),
                                        8.,
                                    ),
                                    self.rich_body(
                                        format!("{key}/body"),
                                        Message::OpenMessageLink,
                                        note.blocks.clone(),
                                    ),
                                ],
                            ),
                            4.,
                        ),
                        Some(wire::Length::Fill),
                        None,
                    ),
                ],
            ),
            8.,
        )
    }
}
