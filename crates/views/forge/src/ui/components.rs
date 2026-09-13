use super::forge::{action, primary, section, subtle};
use super::kit::state_tone;
use super::*;
use crate::host;
use ducktape_view_guest::{kit::Tone, slots};

impl ForgeView {
    /// Where the tree pane stands: the root, then one pressable segment per
    /// directory down to the open one.
    fn tree_crumb(&self) -> wire::Node {
        let p = native::palette();
        // a crumb segment: a quiet button, tight, never underlined
        let segment = |key: String, name: &str, path: String| {
            let mut button = subtle(key, name, Some(Message::ForgeOpenDir(path)));
            if let wire::Node::Button { padding, .. } = &mut button {
                *padding = Some(wire::Edges {
                    top: 2.,
                    right: 6.,
                    bottom: 2.,
                    left: 6.,
                });
            }
            button
        };
        let mut root = segment("forge/tree-root".into(), "root", String::new());
        if let wire::Node::Button { label, .. } = &mut root {
            *label = Some("Back to the repository root".into());
        }
        let mut crumb = vec![root];
        let mut walked = String::new();
        for name in self.tree_path.split('/').filter(|s| !s.is_empty()) {
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(name);
            crumb.push(native::nowrap(native::colored(
                native::caption(format!("forge/tree-crumb/{walked}/slash"), "/"),
                p.faint,
            )));
            crumb.push(segment(
                format!("forge/tree-crumb/{walked}"),
                name,
                walked.clone(),
            ));
        }
        let mut row = native::spaced(native::wrapped_row("forge/tree-head", crumb), 2.);
        if let wire::Node::Linear { padding, .. } = &mut row {
            *padding = Some(wire::Edges {
                top: 4.,
                right: 4.,
                bottom: 2.,
                left: 4.,
            });
        }
        row
    }

    pub(super) fn code_screen(&self) -> wire::Node {
        let mut tree = vec![self.tree_crumb()];
        match self.tree_phase.as_str() {
            "loading" => tree.push(native::secondary(
                "forge/tree-loading",
                "Loading repository tree…",
            )),
            "failed" => tree.push(native::wrapping(native::secondary(
                "forge/tree-failed",
                "Could not load the tree. Open the repository again to retry.",
            ))),
            "ready" => {
                for entry in &self.tree_entries {
                    let directory = entry.kind == "dir";
                    let route = if directory {
                        Message::ForgeOpenDir(entry.path.clone())
                    } else {
                        Message::ForgeOpenFile(entry.path.clone())
                    };
                    let name = native::nowrap(native::text(
                        format!("forge/tree/{}/name", entry.path),
                        if directory {
                            format!("{}/", entry.name)
                        } else {
                            entry.name.clone()
                        },
                    ));
                    let name = if directory {
                        native::weighted(name, wire::Weight::Medium)
                    } else {
                        name
                    };
                    // a kit button centres a shrink-width child: the row
                    // fills so the name sits at the left
                    let line = native::row(format!("forge/tree/{}/line", entry.path), [name]);
                    let mut button = native::list_row(
                        format!("forge/tree/{}", entry.path),
                        line,
                        entry.path == self.file_path,
                        Some(slots::message(route)),
                    );
                    if let wire::Node::Button {
                        description, label, ..
                    } = &mut button
                    {
                        *description = Some(entry.path.clone());
                        *label = Some(entry.name.clone());
                    }
                    tree.push(button);
                }
                if self.tree_entries.is_empty() {
                    let empty = if !self.tree_born {
                        "This repository has no commits yet."
                    } else {
                        "This directory is empty."
                    };
                    tree.push(native::wrapping(native::secondary(
                        "forge/tree-empty",
                        empty,
                    )));
                }
                if self.tree_truncated {
                    tree.push(native::caption(
                        "forge/tree-omitted",
                        "Some directory entries are not shown.",
                    ));
                }
            }
            _ => {}
        }
        let pane = native::pane(
            "forge/tree-pane",
            native::scroll(
                "forge/tree-scroll",
                native::padded(
                    native::spaced(native::column("forge/tree", tree), 1.),
                    wire::Edges::all(6.),
                ),
            ),
            wire::Length::Fixed(self.tree_width as f32),
        );
        let resize = wire::Node::ResizeHandle {
            key: "forge/tree-resize".into(),
            on_press: None,
            on_release: None,
            on_drag: Some(slots::handler::<(f64, f64), Message>(Box::new(|(x, y)| {
                Some(Message::TreeResized(x, y))
            }))),
            cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
            // the hairline shows; the 10px grip is what the pointer lands on
            content: Box::new(native::sized(
                native::container(
                    "forge/tree-grip",
                    native::vertical_divider("forge/tree-edge"),
                ),
                Some(wire::Length::Fixed(10.)),
                Some(wire::Length::Fill),
            )),
        };
        // the split runs to the edges of the content area: the pane's own
        // hairline is the only frame it gets
        let mut split = native::sized(
            native::spaced(
                native::row(
                    "forge/code",
                    [
                        pane,
                        resize,
                        self.file_pane(),
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

    /// The reader beside the tree: a header strip naming the open file over
    /// its text, or the invitation to choose one.
    fn file_pane(&self) -> wire::Node {
        let path = host::forge_file_header(
            &self.opened_dir,
            &self.opened_rev,
            &self.tree_path,
            &self.tree_rev,
            &self.file_path,
        );
        // no file open: the tree pane already says why when it is still
        // loading, failed, or lists nothing, so this pane speaks only when
        // there is something to choose
        if path.is_empty() {
            let choosable = self.tree_phase == "ready"
                && (!self.tree_entries.is_empty() || self.tree_truncated);
            let mut content = Vec::new();
            if choosable {
                content.push(native::empty_state(
                    "forge/choose-file",
                    "No file open",
                    "Choose a file from the tree.",
                ));
            }
            return native::scroll(
                "forge/file-scroll",
                native::padded(native::column("forge/file", content), wire::Edges::all(8.)),
            );
        }
        let head = native::sized(
            native::padded(
                native::spaced(
                    native::centered_row(
                        "forge/file-head",
                        [
                            native::sized(
                                native::nowrap(native::mono("forge/file-header", &path)),
                                Some(wire::Length::Fill),
                                None,
                            ),
                            native::nowrap(native::caption("forge/code-context", "Read only")),
                        ],
                    ),
                    12.,
                ),
                wire::Edges {
                    top: 0.,
                    right: 12.,
                    bottom: 0.,
                    left: 12.,
                },
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(32.)),
        );
        native::sized(
            native::spaced(
                native::column(
                    "forge/file-column",
                    [
                        head,
                        native::divider("forge/file-head-edge"),
                        native::scroll(
                            "forge/file-scroll",
                            native::padded(self.file_body(), wire::Edges::all(12.)),
                        ),
                    ],
                ),
                0.,
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }

    fn file_body(&self) -> wire::Node {
        let mut content = Vec::new();
        match self.file_phase.as_str() {
            "loading" => content.push(native::secondary("forge/loading-file", "Loading file…")),
            "failed" => content.push(native::tone_text(
                "forge/file-failed",
                &self.file_note,
                Tone::Danger,
            )),
            "ready" => {
                if self.file_binary {
                    content.push(native::secondary(
                        "forge/binary",
                        host::binary_note(&self.file_text),
                    ));
                } else if self.file_picture {
                    content.push(wire::Node::Surface {
                        key: "forge/file-picture".into(),
                        name: "picture".into(),
                        args: vec![
                            wire::SurfaceValue::Str("forge".into()),
                            wire::SurfaceValue::Str(self.file_path.clone()),
                        ],
                        on_event: None,
                    });
                    content.push(native::caption(
                        "forge/picture-caption",
                        host::picture_caption(self.file_width, self.file_height),
                    ));
                } else {
                    let markdown = host::markdown_path(&self.file_path);
                    content.push(wire::Node::Surface {
                        key: "forge/file-text".into(),
                        name: if markdown {
                            "forge_markdown"
                        } else {
                            "forge_code"
                        }
                        .into(),
                        args: vec![
                            wire::SurfaceValue::Str(self.file_text.clone()),
                            wire::SurfaceValue::Str(self.file_path.clone()),
                            wire::SurfaceValue::Bool(self.dark),
                        ],
                        on_event: markdown.then(|| {
                            slots::handler(Box::new(|value| match value {
                                wire::SurfaceValue::Str(link) => {
                                    Some(Message::OpenMessageLink(link))
                                }
                                _ => None,
                            }))
                        }),
                    });
                }
                if self.file_truncated {
                    content.push(native::caption(
                        "forge/file-truncated",
                        "This file is larger than the 64 KiB preview limit.",
                    ));
                }
                if !self.file_note.is_empty() {
                    content.push(native::caption("forge/file-note", &self.file_note));
                }
            }
            _ => {}
        }
        native::spaced(native::column("forge/file", content), 10.)
    }

    pub(super) fn diff_screen(&self) -> wire::Node {
        let p = native::palette();
        let title = native::centered_row(
            "forge/diff-head",
            [
                native::sized(
                    native::heading("forge/diff-title", "Changes"),
                    Some(wire::Length::Fill),
                    None,
                ),
                native::nowrap(native::secondary(
                    "forge/diff-count",
                    host::forge_stats(
                        self.forge_item_files_changed,
                        self.forge_item_additions,
                        self.forge_item_deletions,
                    ),
                )),
            ],
        );
        let number = |key: String, value: &str| {
            native::sized(
                native::nowrap(native::colored(native::mono(key, value), p.faint)),
                Some(wire::Length::Fixed(40.)),
                None,
            )
        };
        let render = |line: &host::DiffLine| {
            let key = format!("forge/diff/{}", line.key);
            match line.kind.as_str() {
                "note" => native::padded(
                    native::row(
                        &key,
                        [native::nowrap(native::caption(
                            format!("{key}/text"),
                            &line.text,
                        ))],
                    ),
                    wire::Edges {
                        top: 2.,
                        right: 8.,
                        bottom: 2.,
                        left: 8.,
                    },
                ),
                "file" => {
                    let mut head = native::padded(
                        native::row(
                            &key,
                            [native::nowrap(native::weighted(
                                native::mono(format!("{key}/text"), &line.text),
                                wire::Weight::Medium,
                            ))],
                        ),
                        wire::Edges {
                            top: 4.,
                            right: 8.,
                            bottom: 4.,
                            left: 8.,
                        },
                    );
                    if let wire::Node::Linear { background, .. } = &mut head {
                        *background = Some(native::rgba(p.surface_raised));
                    }
                    head
                }
                "hunk" => native::padded(
                    native::row(
                        &key,
                        [native::nowrap(native::colored(
                            native::mono(format!("{key}/text"), &line.text),
                            p.link,
                        ))],
                    ),
                    wire::Edges {
                        top: 2.,
                        right: 8.,
                        bottom: 2.,
                        left: 8.,
                    },
                ),
                _ => {
                    let line_number = if line.side == "old" {
                        &line.old_no
                    } else {
                        &line.new_no
                    };
                    let (color, wash) = match line.sign.as_str() {
                        "+" => (p.success, Some(p.success_soft)),
                        "-" => (p.danger, Some(p.danger_soft)),
                        _ => (p.foreground, None),
                    };
                    let mut cells = vec![
                        number(format!("{key}/old"), &line.old_no),
                        number(format!("{key}/new"), &line.new_no),
                        native::sized(
                            native::nowrap(native::colored(
                                native::mono(format!("{key}/sign"), &line.sign),
                                color,
                            )),
                            Some(wire::Length::Fixed(14.)),
                            None,
                        ),
                        native::sized(
                            native::nowrap(native::colored(
                                native::mono(format!("{key}/text"), &line.text),
                                color,
                            )),
                            Some(wire::Length::Fill),
                            None,
                        ),
                    ];
                    // a `+` at the end of every commentable row; the words
                    // ride as the accessible label a test presses
                    if !line.path.is_empty() {
                        let mut comment = native::button(
                            format!("{key}/comment"),
                            "+",
                            Some(slots::message(Message::ForgeCommentOpen(
                                line.path.clone(),
                                line_number.clone(),
                                line.side.clone(),
                            ))),
                            wire::ButtonPreset::Subtle,
                        );
                        if let wire::Node::Button {
                            label, padding, ..
                        } = &mut comment
                        {
                            *label = Some("Comment on this line".into());
                            *padding = Some(wire::Edges {
                                top: 0.,
                                right: 6.,
                                bottom: 0.,
                                left: 6.,
                            });
                        }
                        cells.push(comment);
                    }
                    let mut row = native::padded(
                        native::sized(
                            native::centered_row(key, cells),
                            None,
                            Some(wire::Length::Fixed(24.)),
                        ),
                        wire::Edges {
                            top: 0.,
                            right: 8.,
                            bottom: 0.,
                            left: 8.,
                        },
                    );
                    if let wire::Node::Linear { background, .. } = &mut row {
                        *background = wash.map(native::rgba);
                    }
                    row
                }
            }
        };
        // one block per changed file: a `file` line opens the next one, and
        // each block wears its own hairline.
        let mut blocks: Vec<Vec<&host::DiffLine>> = Vec::new();
        for line in &self.diff_rows {
            let opens_a_block = line.kind == "file" || blocks.is_empty();
            if opens_a_block {
                blocks.push(Vec::new());
            }
            blocks.last_mut().expect("a block is open").push(line);
        }
        let mut content: Vec<wire::Node> = blocks
            .into_iter()
            .map(|block| wire::Node::KeyedColumn {
                key: format!("forge/diff-lines/{}", block[0].key),
                keys: Some(
                    block
                        .iter()
                        .map(|line| wire::ListKey::from(line.key))
                        .collect(),
                ),
                children: block.into_iter().map(&render).collect(),
                background: Some(native::rgba(p.surface)),
                border: Some(wire::Border {
                    color: Some(native::rgba(p.border)),
                    width: Some(1.),
                    radius: Some([native::radius::CONTROL as f32; 4]),
                }),
                spacing: None,
                padding: None,
                width: Some(wire::Length::Fill),
                height: None,
                max_width: None,
                align: None,
                virtual_row: Some(24.),
            })
            .collect();
        if self.forge_item_diff_truncated {
            content.push(native::caption(
                "forge/diff-truncated",
                "This diff is truncated; open the repository locally to see the rest.",
            ));
        }
        // no rows: either the patch read was refused (then nothing pins a
        // source head and the merge and review doors stay shut) or the two
        // tips are identical
        if content.is_empty() {
            let unloaded = self.forge_item_source_oid.is_empty();
            content.push(native::wrapping(native::secondary(
                "forge/no-diff",
                if unloaded {
                    "The changes could not be loaded. Open the pull request again to retry."
                } else {
                    "No changes between the two branches."
                },
            )));
        }
        section("forge/diff", title, content)
    }

    /// The merge door, as the properties rail carries it: the review tally,
    /// the one button, and what stopped the last attempt.
    pub(super) fn merge_screen(&self) -> wire::Node {
        let mut content = Vec::new();
        match self.forge_item_state.as_str() {
            "merged" => content.push(native::notice(
                "forge/merged-box",
                native::wrapping(native::text(
                    "forge/merged",
                    host::forge_merge_note(&self.forge_item_merge_oid, &self.forge_item_branches),
                )),
                Tone::Success,
            )),
            "closed" => content.push(native::secondary(
                "forge/closed",
                "This pull request is closed.",
            )),
            "open" => {
                let mut tally = vec![native::badge(
                    "forge/approvals",
                    host::plural(self.forge_item_approvals, "approval", "approvals"),
                    if self.forge_item_approvals > 0 {
                        Tone::Success
                    } else {
                        Tone::Neutral
                    },
                )];
                if self.forge_item_change_requests > 0 {
                    tally.push(native::badge(
                        "forge/changes-requested",
                        format!(
                            "{} requested changes",
                            host::plural(self.forge_item_change_requests, "reviewer", "reviewers")
                        ),
                        Tone::Danger,
                    ));
                }
                content.push(native::spaced(
                    native::wrapped_row("forge/merge-tally", tally),
                    6.,
                ));
                let available =
                    self.connected && !self.merge_busy && !self.forge_item_source_oid.is_empty();
                content.push(native::sized(
                    primary(
                        "forge/merge",
                        if self.merge_busy {
                            "Merging…"
                        } else {
                            "Merge pull request"
                        },
                        available.then_some(Message::ForgeMergeSubmit),
                    ),
                    Some(wire::Length::Fill),
                    None,
                ));
                if self.forge_item_change_requests > 0 {
                    content.push(native::wrapping(native::tone_text(
                        "forge/merge-warning",
                        "Merging is not recommended while changes are requested.",
                        Tone::Danger,
                    )));
                }
                if !self.merge_conflicts.is_empty() {
                    let mut conflicts = vec![native::wrapping(native::text(
                        "forge/conflict-title",
                        "Merge conflicts — resolve on the branch and push again:",
                    ))];
                    for path in &self.merge_conflicts {
                        conflicts.push(native::wrapping(native::mono(
                            format!("forge/conflict/{path}"),
                            path,
                        )));
                    }
                    content.push(native::notice(
                        "forge/conflicts",
                        native::spaced(native::column("forge/conflict-list", conflicts), 4.),
                        Tone::Warning,
                    ));
                }
            }
            _ => {}
        }
        native::card(
            "forge/merge-box",
            native::spaced(native::column("forge/merge-content", content), 10.),
        )
    }

    pub(super) fn review_screen(&self) -> wire::Node {
        let mut content = Vec::new();
        for (index, review) in self.forge_item_reviews.iter().enumerate() {
            let key = format!("forge/review/{index}");
            if index > 0 {
                content.push(native::divider(format!("{key}/edge")));
            }
            let mut head = vec![
                native::nowrap(native::strong(format!("{key}/author"), &review.author_name)),
                native::badge(
                    format!("{key}/verdict"),
                    host::verdict_label(&review.verdict),
                    state_tone(&review.verdict),
                ),
                native::nowrap(native::colored(
                    native::mono(format!("{key}/commit"), &review.commit),
                    native::palette().muted,
                )),
                self.finality(format!("{key}/finality")),
            ];
            if review.outdated {
                head.push(native::badge(
                    format!("{key}/outdated"),
                    "Outdated",
                    Tone::Warning,
                ));
            }
            let mut details = vec![
                native::spaced(native::wrapped_row(format!("{key}/head"), head), 8.),
                self.rich_body(
                    format!("{key}/body"),
                    Message::OpenMessageLink,
                    review.blocks.clone(),
                ),
            ];
            for (index, comment) in review.comments.iter().enumerate() {
                let mut boxed = native::padded(
                    native::spaced(
                        native::column(
                            format!("{key}/comment/{index}"),
                            [
                                native::nowrap(native::mono(
                                    format!("{key}/comment/{index}/anchor"),
                                    &comment.anchor,
                                )),
                                self.rich_body(
                                    format!("{key}/comment/{index}/body"),
                                    Message::OpenMessageLink,
                                    comment.blocks.clone(),
                                ),
                            ],
                        ),
                        4.,
                    ),
                    wire::Edges {
                        top: 4.,
                        right: 0.,
                        bottom: 4.,
                        left: 12.,
                    },
                );
                if let wire::Node::Linear { border, .. } = &mut boxed {
                    *border = Some(wire::Border {
                        color: Some(native::rgba(native::palette().border_strong)),
                        width: Some(1.),
                        radius: None,
                    });
                }
                details.push(boxed);
            }
            content.push(native::spaced(
                native::column(format!("{key}/details"), details),
                8.,
            ));
        }
        let available = self.connected && !self.review_busy;
        let mut compose = vec![native::spaced(
            native::row(
                "forge/verdicts",
                [
                    ("comment", "Comment"),
                    ("approve", "Approve"),
                    ("request_changes", "Request changes"),
                ]
                .into_iter()
                .map(|(value, label)| {
                    let mut button = subtle(
                        format!("forge/verdict/{value}"),
                        label,
                        available.then(|| Message::ForgeReviewPick(value.into())),
                    );
                    if let wire::Node::Button { checked, .. } = &mut button {
                        *checked = Some(self.review_verdict == value);
                    }
                    if let wire::Node::Button { label, .. } = &mut button {
                        *label = Some(format!("Pick {} verdict", value.replace('_', " ")));
                    }
                    button
                }),
            ),
            2.,
        )];
        let target =
            host::forge_comment_target(&self.comment_path, &self.comment_line, &self.comment_side);
        let comment_capacity = !host::forge_comment_cap_reached(&self.staged_comments);
        if !target.is_empty() {
            let submit = available && comment_capacity && !self.comment_draft.is_empty();
            compose.push(native::notice(
                "forge/comment-box",
                native::spaced(
                    native::column(
                        "forge/comment-form",
                        [
                            native::centered_row(
                                "forge/comment-target",
                                [
                                    native::sized(
                                        native::nowrap(native::mono(
                                            "forge/comment-anchor",
                                            target,
                                        )),
                                        Some(wire::Length::Fill),
                                        None,
                                    ),
                                    subtle(
                                        "forge/cancel-comment",
                                        "Cancel",
                                        Some(Message::ForgeCommentCancel),
                                    ),
                                ],
                            ),
                            native::spaced(
                                native::row(
                                    "forge/comment-row",
                                    [
                                        self.review_input(
                                            "forge/comment-body",
                                            "Comment on this line…",
                                            &self.comment_draft,
                                            Message::CommentDraftChanged,
                                            submit.then(|| {
                                                Message::ForgeCommentStage(
                                                    self.comment_draft.clone(),
                                                )
                                            }),
                                        ),
                                        action(
                                            "forge/add-comment",
                                            "Add comment",
                                            submit.then(|| {
                                                Message::ForgeCommentStage(
                                                    self.comment_draft.clone(),
                                                )
                                            }),
                                        ),
                                    ],
                                ),
                                6.,
                            ),
                        ],
                    ),
                    6.,
                ),
                Tone::Accent,
            ));
        }
        if !comment_capacity {
            compose.push(native::wrapping(native::tone_text(
                "forge/comment-limit",
                "Comment limit reached for one review — submit this review, then start another.",
                Tone::Warning,
            )));
        }
        for (index, comment) in self.staged_comments.iter().enumerate() {
            let key = format!("forge/staged/{index}");
            compose.push(native::card(
                &key,
                native::spaced(
                    native::column(
                        format!("{key}/body-column"),
                        [
                            native::spaced(
                                native::centered_row(
                                    format!("{key}/head"),
                                    [
                                        native::sized(
                                            native::nowrap(native::mono(
                                                format!("{key}/anchor"),
                                                &comment.anchor,
                                            )),
                                            Some(wire::Length::Fill),
                                            None,
                                        ),
                                        native::badge(
                                            format!("{key}/status"),
                                            "Not sent yet",
                                            Tone::Warning,
                                        ),
                                        subtle(
                                            format!("{key}/remove"),
                                            "Remove staged comment",
                                            Some(Message::ForgeCommentDrop(comment.anchor.clone())),
                                        ),
                                    ],
                                ),
                                8.,
                            ),
                            native::wrapping(native::text(format!("{key}/body"), &comment.body)),
                        ],
                    ),
                    4.,
                ),
            ));
        }
        let submit = available
            && !self.forge_item_source_oid.is_empty()
            && (!self.review_draft.is_empty() || !self.staged_comments.is_empty());
        compose.push(native::spaced(
            native::row(
                "forge/review-row",
                [
                    self.review_input(
                        "forge/review-body",
                        "Leave a review…",
                        &self.review_draft,
                        Message::ReviewDraftChanged,
                        submit.then(|| Message::ForgeReviewSubmit(self.review_draft.clone())),
                    ),
                    primary(
                        "forge/submit-review",
                        if self.review_busy {
                            "Sending…"
                        } else {
                            "Submit review"
                        },
                        submit.then(|| Message::ForgeReviewSubmit(self.review_draft.clone())),
                    ),
                ],
            ),
            6.,
        ));
        content.push(native::card(
            "forge/compose",
            native::spaced(native::column("forge/compose-column", compose), 8.),
        ));
        section(
            "forge/reviews",
            native::heading("forge/reviews-title", "Reviews"),
            content,
        )
    }

    fn review_input(
        &self,
        key: &str,
        hint: &str,
        value: &str,
        route: fn(String) -> Message,
        submit: Option<Message>,
    ) -> wire::Node {
        let mut input = native::input(
            key,
            hint,
            value,
            slots::handler(Box::new(move |value| Some(route(value)))),
            submit.map(slots::message),
        );
        if let wire::Node::Input { options, width, .. } = &mut input {
            options.disabled = self.review_busy || !self.connected;
            *width = Some(wire::Length::Fill);
        }
        input
    }
}
