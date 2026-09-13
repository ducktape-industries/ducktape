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

impl ForgeView {
    pub(super) fn forge_screen(&self) -> wire::Node {
        if !self.connected {
            return self.disconnected("forge/disconnected".into());
        }
        let mut content = Vec::new();
        if !self.host_error.is_empty() {
            content.push(self.unavailable("forge/error".into()));
        }
        if self.open_repo.is_empty() {
            content.push(native::centered_row(
                "forge/head",
                [
                    native::sized(
                        native::title("forge/organization", &self.org),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    native::badge("forge/tier", &self.tier, Tone::Neutral),
                    native::secondary(
                        "forge/repo-count",
                        host::plural(self.repos.len() as i64, "repository", "repositories"),
                    ),
                ],
            ));
            if !self.about.is_empty() {
                content.push(native::wrapping(native::secondary("forge/about", &self.about)));
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
                content.push(native::wrapping(native::secondary("forge/list-status", label)));
                if self.list_phase == "ready" {
                    let command = host::forge_push_command(&self.connected_rpc);
                    content.push(native::card(
                        "forge/push-card",
                        native::spaced(
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
                                        Some(Message::CopyToClipboard(
                                            command,
                                            "Command copied".into(),
                                        )),
                                    ),
                                ],
                            ),
                            12.,
                        ),
                    ));
                }
            }
            let mut rows = Vec::new();
            for repo in &self.repos {
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
                                native::mono(format!("forge/head/{}", repo.name), &repo.head),
                                native::palette().muted,
                            )),
                        ],
                    ),
                    false,
                    Some(slots::message(Message::ForgeOpenRepo(repo.name.clone()))),
                );
                if let wire::Node::Button { label, .. } = &mut row {
                    *label = Some(repo.name.clone());
                }
                rows.push(row);
            }
            if !rows.is_empty() {
                let mut card = native::card(
                    "forge/repo-card",
                    native::spaced(native::column("forge/repo-rows", rows), 1.),
                );
                if let wire::Node::Container { padding, .. } = &mut card {
                    *padding = Some(wire::Edges::all(6.));
                }
                content.push(card);
            }
            return native::scroll(
                "forge/repositories",
                native::spaced(native::column("forge/list", content), 16.),
            );
        }
        let mut header = vec![
            subtle("forge/all-repos", "All repos", Some(Message::ForgeCloseRepo)),
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
        let tabs = [
            ("code", "Code"),
            ("pulls", "Pull requests"),
            ("issues", "Issues"),
        ];
        let tab_buttons = tabs.into_iter().map(|(tab, label)| {
            let kind = match tab {
                "pulls" => "pr",
                "issues" => "issue",
                _ => "",
            };
            let text = if kind.is_empty() {
                label.to_owned()
            } else {
                format!("{label}  {}", host::forge_open_count(&self.items, kind))
            };
            let mut button = subtle(
                format!("forge/tab/{tab}"),
                &text,
                (self.tab != tab || self.forge_item_number > 0)
                    .then(|| Message::SelectForgeTab(tab.into())),
            );
            if let wire::Node::Button { checked, label, .. } = &mut button {
                *checked = Some(self.tab == tab);
                *label = Some(
                    match tab {
                        "code" => "Browse the code",
                        "pulls" => "Show pull requests",
                        _ => "Show issues",
                    }
                    .into(),
                );
            }
            button
        });
        header.push(native::spacer());
        header.extend(tab_buttons);
        content.push(native::spaced(
            native::centered_row("forge/navigation", header),
            6.,
        ));
        let body = if self.forge_item_number > 0 {
            self.item_screen()
        } else {
            match self.tab.as_str() {
                "code" => self.code_screen(),
                "issues" => self.tracker_screen("issues"),
                _ => self.tracker_screen("pulls"),
            }
        };
        content.push(body);
        native::sized(
            native::spaced(native::column("forge/root", content), 12.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }

    fn tracker_screen(&self, tab: &str) -> wire::Node {
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
                let mut rows = Vec::new();
                for item in items {
                    let key = format!("forge/item/{}", item.number);
                    let line = native::centered_row(
                        format!("{key}/line"),
                        [
                            native::sized(
                                native::spaced(
                                    native::column(
                                        format!("{key}/lines"),
                                        [
                                            native::nowrap(native::strong(
                                                format!("{key}/open"),
                                                &item.title,
                                            )),
                                            native::spaced(
                                                native::centered_row(
                                                    format!("{key}/meta"),
                                                    [
                                                        native::nowrap(native::mono(
                                                            format!("{key}/number"),
                                                            format!("#{}", item.number),
                                                        )),
                                                        native::nowrap(native::caption(
                                                            format!("{key}/author"),
                                                            &item.author_name,
                                                        )),
                                                    ],
                                                ),
                                                8.,
                                            ),
                                        ],
                                    ),
                                    2.,
                                ),
                                Some(wire::Length::Fill),
                                None,
                            ),
                            native::badge(
                                format!("{key}/state"),
                                &item.state,
                                state_tone(&item.state),
                            ),
                        ],
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
                    rows.push(row);
                }
                if !rows.is_empty() {
                    let mut card = native::card(
                        "forge/tracker-card",
                        native::spaced(native::column("forge/tracker-rows", rows), 1.),
                    );
                    if let wire::Node::Container { padding, .. } = &mut card {
                        *padding = Some(wire::Edges::all(6.));
                    }
                    content.push(card);
                }
            }
            _ => {}
        }
        native::scroll(
            "forge/tracker",
            native::spaced(native::column("forge/tracker-content", content), 12.),
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
                        &self.forge_item_state,
                        state_tone(&self.forge_item_state),
                    ),
                    native::nowrap(native::mono(
                        "forge/item-number",
                        format!("#{}", self.forge_item_number),
                    )),
                    native::nowrap(native::secondary("forge/item-author", &self.forge_item_author)),
                ];
                if !self.forge_item_branches.is_empty() {
                    meta.push(native::nowrap(native::mono(
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
                            native::wrapping(native::title("forge/item-title", &self.forge_item_title)),
                            native::spaced(native::wrapped_row("forge/item-meta", meta), 8.),
                        ],
                    ),
                    6.,
                ));
                if !self.forge_item_body.is_empty() {
                    content.push(native::card(
                        "forge/item-body-card",
                        self.item_body("forge/item-body".into(), Message::OpenMessageLink),
                    ));
                }
                if !self.diff_rows.is_empty() {
                    content.push(self.diff_screen());
                }
                if self.forge_item_kind == "pr" {
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
                    discussion.push(native::secondary("forge/no-discussion", "No discussion yet."));
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
        let mut column = native::spaced(native::column("forge/item-content", content), 16.);
        if let wire::Node::Linear { max_width, .. } = &mut column {
            *max_width = Some(920.);
        }
        native::scroll("forge/item-scroll", column)
    }

    fn note(&self, key: String, note: &host::ChatMessage) -> wire::Node {
        let human = note.avatar_kind == "human";
        native::card(
            &key,
            native::spaced(
                native::column(
                    format!("{key}/body-column"),
                    [
                        native::spaced(
                            native::centered_row(
                                format!("{key}/head"),
                                [
                                    native::avatar(
                                        format!("{key}/avatar"),
                                        note.initial.clone(),
                                        if human { Tone::Neutral } else { Tone::Agent },
                                    ),
                                    native::nowrap(native::strong(format!("{key}/author"), &note.author)),
                                    native::nowrap(native::caption(format!("{key}/meta"), &note.meta)),
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
                8.,
            ),
        )
    }
}
