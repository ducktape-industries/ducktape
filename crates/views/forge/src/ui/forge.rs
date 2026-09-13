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
const INSET: f32 = 20.;

impl ForgeView {
    pub(super) fn forge_screen(&self) -> wire::Node {
        if !self.connected {
            return self.disconnected("forge/disconnected".into());
        }
        if self.open_repo.is_empty() {
            return self.namespace_screen();
        }
        self.repo_screen()
    }

    /// Every repository on the network: a title row, then the list. The list
    /// IS the page — no card frames it.
    fn namespace_screen(&self) -> wire::Node {
        let p = native::palette();
        let mut content = Vec::new();
        if !self.host_error.is_empty() {
            content.push(self.unavailable("forge/error".into()));
        }
        let mut head = vec![native::sized(
            native::title("forge/organization", &self.org),
            Some(wire::Length::Fill),
            None,
        )];
        if !self.tier.is_empty() {
            head.push(native::badge(
                "forge/tier",
                host::state_label(&self.tier),
                Tone::Neutral,
            ));
        }
        head.push(native::nowrap(native::secondary(
            "forge/repo-count",
            host::plural(self.repos.len() as i64, "repository", "repositories"),
        )));
        content.push(native::spaced(native::centered_row("forge/head", head), 8.));
        if !self.about.is_empty() {
            content.push(native::wrapping(native::secondary(
                "forge/about",
                &self.about,
            )));
        }
        if self.repos.is_empty() {
            let label = match self.list_phase.as_str() {
                "loading" => "Loading repositories…",
                "failed" => "Could not load repositories. Reconnect to retry.",
                "ready" => {
                    "No repositories yet — push a git repository to this network to create one."
                }
                _ => "",
            };
            content.push(native::wrapping(native::secondary(
                "forge/list-status",
                label,
            )));
            if self.list_phase == "ready" {
                content.push(self.push_box());
            }
        }
        let rows = self.repos.iter().map(|repo| {
            let key = format!("forge/repo/{}", repo.name);
            let mut row = native::list_row(
                &key,
                native::centered_row(
                    format!("{key}/line"),
                    [
                        native::sized(
                            native::strong(format!("forge/open/{}", repo.name), &repo.name),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        native::nowrap(native::colored(
                            native::text_size(
                                native::mono(format!("forge/head/{}", repo.name), &repo.head),
                                native::type_scale::CAPTION as f32,
                            ),
                            p.muted,
                        )),
                    ],
                ),
                false,
                Some(slots::message(Message::ForgeOpenRepo(repo.name.clone()))),
            );
            if let wire::Node::Button { label, .. } = &mut row {
                *label = Some(repo.name.clone());
            }
            row
        });
        if !self.repos.is_empty() {
            content.push(native::spaced(native::column("forge/repo-rows", rows), 1.));
        }
        native::scroll(
            "forge/repositories",
            native::padded(
                native::spaced(native::column("forge/list", content), 12.),
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
        }
        node
    }

    /// One open repository: the toolbar, its hairline, and whichever screen
    /// the tabs stand on.
    fn repo_screen(&self) -> wire::Node {
        let mut content = Vec::new();
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
        content.push(self.toolbar());
        content.push(native::divider("forge/toolbar-edge"));
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

    /// The repository toolbar: where you are on the left, what you are
    /// reading on the right.
    fn toolbar(&self) -> wire::Node {
        let mut header = vec![
            subtle(
                "forge/all-repos",
                "All repos",
                Some(Message::ForgeCloseRepo),
            ),
            native::caption("forge/crumb", "/"),
            picker(
                "ForgeView/forge/repo-pick",
                host::repo_names(&self.repos),
                &self.open_repo,
                None,
                Message::ForgeOpenRepo,
            ),
        ];
        if !self.branches.is_empty() {
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
                native::spaced(native::centered_row("forge/navigation", header), 6.),
                wire::Edges {
                    top: 0.,
                    right: 12.,
                    bottom: 0.,
                    left: 12.,
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

    /// The tracker: one line per item, its state read off the badge at the
    /// left. The list is the screen — no card, no outer inset.
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
                        native::centered_row(
                            format!("{key}/line"),
                            [
                                native::badge(
                                    format!("{key}/state"),
                                    host::state_label(&item.state),
                                    state_tone(&item.state),
                                ),
                                native::sized(
                                    native::nowrap(native::strong(
                                        format!("{key}/open"),
                                        &item.title,
                                    )),
                                    Some(wire::Length::Fill),
                                    None,
                                ),
                                native::nowrap(native::colored(
                                    native::text_size(
                                        native::mono(
                                            format!("{key}/number"),
                                            format!("#{}", item.number),
                                        ),
                                        native::type_scale::CAPTION as f32,
                                    ),
                                    p.muted,
                                )),
                                native::nowrap(native::caption(
                                    format!("{key}/author"),
                                    &item.author_name,
                                )),
                            ],
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
                    top: 8.,
                    right: 12.,
                    bottom: 12.,
                    left: 12.,
                },
            ),
        )
    }

    fn item_screen(&self) -> wire::Node {
        let mut content = vec![native::row(
            "forge/item-nav",
            [subtle(
                "forge/back",
                "Back to tracker",
                Some(Message::ForgeCloseItem),
            )],
        )];
        match self.item_phase.as_str() {
            "loading" => content.push(self.loading_item("forge/item-loading".into())),
            "failed" => content.push(self.item_unavailable("forge/item-failed".into())),
            "ready" => {
                let mut meta = vec![
                    native::badge(
                        "forge/item-state",
                        host::state_label(&self.forge_item_state),
                        state_tone(&self.forge_item_state),
                    ),
                    native::nowrap(native::mono(
                        "forge/item-number",
                        format!("#{}", self.forge_item_number),
                    )),
                    native::nowrap(native::secondary(
                        "forge/item-author",
                        &self.forge_item_author,
                    )),
                ];
                if !self.forge_item_branches.is_empty() {
                    meta.push(native::nowrap(native::secondary(
                        "forge/item-branches",
                        &self.forge_item_branches,
                    )));
                }
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
                content.push(native::spaced(
                    native::column(
                        "forge/item-head",
                        [
                            native::wrapping(native::title(
                                "forge/item-title",
                                &self.forge_item_title,
                            )),
                            native::spaced(native::wrapped_row("forge/item-meta", meta), 8.),
                        ],
                    ),
                    6.,
                ));
                if !self.forge_item_body.is_empty() {
                    content
                        .push(self.item_body("forge/item-body".into(), Message::OpenMessageLink));
                }
                if self.forge_item_kind == "pr" {
                    content.push(self.diff_screen());
                    content.push(self.merge_screen());
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
            }
            _ => {}
        }
        let mut column = native::spaced(
            native::padded(
                native::column("forge/item-content", content),
                wire::Edges::all(INSET),
            ),
            16.,
        );
        if let wire::Node::Linear { max_width, .. } = &mut column {
            *max_width = Some(920.);
        }
        native::scroll("forge/item-scroll", column)
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
