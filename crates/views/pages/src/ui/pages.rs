impl PagesView {
    fn pages(&self) -> Node {
        kit::set_dark(self.document_dark);
        let sidebar = kit::pane(
            format!("{PAGE_KEY}/page-list"),
            self.sidebar(),
            Length::Fixed(self.sidebar_width as f32),
        );
        let divider = Node::ResizeHandle {
            key: format!("{PAGE_KEY}/sidebar-divider"),
            on_press: None,
            on_release: None,
            on_drag: Some(slots::handler(Box::new(|(x, y): (f64, f64)| {
                Some(Message::SidebarResized(x, y))
            }))),
            cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
            // The page list's own border draws the edge; this is the grab strip.
            content: Box::new(Node::Space {
                width: Some(Length::Fixed(4.)),
                height: Some(Length::Fill),
            }),
        };
        let mut body = Vec::new();
        if !self.host_error.is_empty() {
            // the inset goes on a wrapper: padding a notice replaces its own
            body.push(kit::padded(
                kit::column(
                    "pages/host-error-inset",
                    [kit::notice(
                        "pages/host-error-box",
                        kit::wrapping(kit::text("pages/host-error", &self.host_error)),
                        Tone::Danger,
                    )],
                ),
                wire::Edges {
                    top: 12.,
                    right: 16.,
                    bottom: 12.,
                    left: 16.,
                },
            ));
        }
        if self.connected && !self.active_page.is_empty() {
            body.push(self.document_toolbar());
        }
        body.push(measured(
            "PagesView/root/pages/pane-measure",
            self.document_pane(),
            Message::PagesPaneResized,
        ));
        let mut main = fill(kit::spaced(kit::column("pages/main", body), 0.));
        if self.connected && self.page_delete_armed {
            main = Node::Stack {
                padding: None,
                background: None,
                border: None,
                clip: false,
                key: "pages/delete-stack".into(),
                width: Some(Length::Fill),
                height: Some(Length::Fill),
                under: 1,
                children: vec![main, self.delete_dialog()],
            };
        }
        fill(kit::spaced(
            kit::row(PAGE_KEY, [sidebar, divider, main]),
            0.,
        ))
    }

    fn sidebar(&self) -> Node {
        if !self.connected {
            return fill(kit::spaced(
                kit::column(
                    "pages/sidebar",
                    [
                        header_bar(
                            "pages/sidebar/header",
                            12.,
                            [kit::nowrap(kit::heading("pages/sidebar/title", "Pages"))],
                        ),
                        kit::divider("pages/sidebar/rule"),
                    ],
                ),
                0.,
            ));
        }
        let mut rows = vec![
            header_bar(
                "pages/sidebar/header",
                12.,
                [
                    kit::nowrap(kit::heading("pages/sidebar/title", "Pages")),
                    kit::badge(
                        "pages/sidebar/count",
                        self.pages.len().to_string(),
                        Tone::Neutral,
                    ),
                    kit::spacer(),
                    action(
                        "pages/sidebar/new",
                        if self.page_create_open {
                            "Cancel"
                        } else {
                            "New page"
                        },
                        Message::TogglePageCreate,
                        !self.unavailable(),
                        ButtonPreset::Subtle,
                    ),
                ],
            ),
            kit::divider("pages/sidebar/rule"),
        ];
        if self.page_create_open {
            rows.push(kit::padded(
                kit::spaced(
                    kit::column(
                        "pages/sidebar/create-form",
                        [
                            input(
                                format!("{PAGE_KEY}/new-page"),
                                "New page",
                                &self.page_draft,
                                Message::PageDraftChanged,
                                Some(Message::CreatePageSubmit),
                                self.unavailable(),
                            ),
                            kit::sized(
                                action(
                                    "pages/sidebar/create",
                                    "Create page",
                                    Message::CreatePageSubmit,
                                    !self.unavailable() && !self.page_draft.trim().is_empty(),
                                    ButtonPreset::Primary,
                                ),
                                Some(Length::Fill),
                                None,
                            ),
                        ],
                    ),
                    6.,
                ),
                wire::Edges {
                    top: 8.,
                    right: 12.,
                    bottom: 12.,
                    left: 12.,
                },
            ));
        }
        let mut list: Vec<Node> = self.pages.iter().map(|page| self.page_button(page)).collect();
        if list.is_empty() && !self.loading {
            list.push(kit::padded(
                kit::column(
                    "pages/sidebar/empty-box",
                    [kit::wrapping(kit::secondary(
                        "pages/sidebar/empty",
                        "No pages yet. Create one to start writing.",
                    ))],
                ),
                wire::Edges::all(8.),
            ));
        }
        rows.push(kit::scroll(
            "pages/sidebar/scroll",
            kit::padded(
                kit::spaced(kit::column("pages/sidebar/list", list), 1.),
                wire::Edges {
                    top: 6.,
                    right: 6.,
                    bottom: 8.,
                    left: 6.,
                },
            ),
        ));
        fill(kit::spaced(kit::column("pages/sidebar", rows), 0.))
    }

    fn document_toolbar(&self) -> Node {
        let title = if self.active_page_title.is_empty() {
            "Untitled"
        } else {
            &self.active_page_title
        };
        let mut crumb = Vec::new();
        if !self.active_page_parent.is_empty() {
            crumb.push(kit::nowrap(kit::secondary(
                "pages/toolbar/parent",
                &self.active_page_parent,
            )));
            crumb.push(kit::nowrap(kit::caption("pages/toolbar/crumb", "/")));
        }
        crumb.push(kit::nowrap(kit::strong("pages/toolbar/title", title)));
        let status = match self.autosave.as_str() {
            "saving" => ("Saving…", Tone::Neutral),
            "error" => ("Not saved", Tone::Danger),
            _ => ("Saved", Tone::Neutral),
        };
        let mut comments = named(
            kit::button(
                "pages/toolbar/comments",
                format!("Comments {}", self.thread_total),
                (!self.unavailable()).then(|| slots::message(Message::ToggleBlockComments)),
                ButtonPreset::Subtle,
            ),
            "Comments",
        );
        if let Node::Button { checked, .. } = &mut comments {
            *checked = Some(self.block_comments_open);
        }
        let mut controls = vec![
            kit::nowrap(kit::text_size(
                kit::tone_text("pages/toolbar/save-status", status.0, status.1),
                kit::type_scale::CAPTION as f32,
            )),
            kit::sized(
                input(
                    format!("{PAGE_KEY}/page-search"),
                    "Search pages…",
                    &self.page_search_draft,
                    Message::SearchDraftChanged,
                    Some(Message::SearchPagesSubmit),
                    !self.host_error.is_empty() || self.page_searching,
                ),
                Some(Length::Fixed(180.)),
                None,
            ),
            comments,
            action(
                "pages/toolbar/link",
                "Copy page link",
                Message::CopyToClipboard(self.page_link.clone(), "Page link".into()),
                !self.page_link.is_empty(),
                ButtonPreset::Subtle,
            ),
            action(
                "pages/toolbar/menu",
                "Page actions",
                Message::TogglePageMenu,
                !self.unavailable(),
                ButtonPreset::Subtle,
            ),
        ];
        // a standing search is cleared from the toolbar: the result panel
        // only shows while the draft still equals the query, so an edited
        // draft would otherwise strand the subscription
        let searching = !self.page_search_draft.is_empty() || !self.page_search_query.is_empty();
        if searching {
            controls.insert(
                2,
                action(
                    "pages/toolbar/clear-search",
                    "Clear search",
                    Message::ClearPageSearch,
                    true,
                    ButtonPreset::Text,
                ),
            );
        }
        kit::spaced(
            kit::column(
                "pages/toolbar-bar",
                [
                    header_bar(
                        "pages/toolbar",
                        16.,
                        [
                            kit::sized(
                                kit::spaced(kit::centered_row("pages/toolbar/heading", crumb), 6.),
                                Some(Length::Fill),
                                None,
                            ),
                            kit::sized(
                                kit::spaced(
                                    kit::centered_row("pages/toolbar/controls", controls),
                                    6.,
                                ),
                                Some(Length::Shrink),
                                None,
                            ),
                        ],
                    ),
                    kit::divider("pages/toolbar/rule"),
                ],
            ),
            0.,
        )
    }

    fn document_pane(&self) -> Node {
        let surface = if !self.connected {
            fill(empty_state(
                "pages/disconnected",
                "Not connected",
                "Connect to a network to read and edit pages.",
            ))
        } else if self.active_page.is_empty() {
            if self.loading {
                fill(empty_state(
                    "pages/loading",
                    "Loading pages…",
                    "Your documents will appear here.",
                ))
            } else {
                fill(empty_state(
                    "pages/selection",
                    "Choose a page",
                    "Select a page on the left, or create a new one.",
                ))
            }
        } else {
            self.document_surface()
        };
        let mut children = vec![surface];
        let search_ready = self.connected
            && crate::host::search_answer_stands(
                &self.page_search_query,
                &self.page_search_draft,
                self.page_searching,
            );
        if search_ready {
            children.push(self.search_panel());
        }
        if self.connected && !self.active_page.is_empty() && self.block_comments_open {
            children.push(self.comments_layer());
        }
        if self.connected && self.page_menu_open {
            let menu = modal(
                "pages/menu/card",
                kit::sized(
                    action(
                        "pages/menu/delete",
                        "Delete page",
                        Message::ArmPageDelete,
                        !self.unavailable(),
                        ButtonPreset::Danger,
                    ),
                    Some(Length::Fill),
                    None,
                ),
                200.,
            );
            let mut menu = menu;
            if let Node::Container { padding, .. } = &mut menu {
                *padding = Some(wire::Edges::all(6.));
            }
            children.push(overlay(
                "pages/menu",
                menu,
                Message::ClosePageMenu,
                wire::AlignX::Right,
                wire::AlignY::Top,
            ));
        }
        Node::Stack {
            padding: None,
            background: None,
            border: None,
            clip: false,
            key: "pages/document/stack".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            under: 1,
            children,
        }
    }

    fn document_surface(&self) -> Node {
        let mut content = Vec::new();
        if !self.page_refusal.is_empty() {
            content.push(kit::notice(
                "pages/document/refusal-box",
                kit::wrapping(kit::text("pages/document/refusal", &self.page_refusal)),
                Tone::Warning,
            ));
        }
        for (index, draft) in self.orphaned_comment_drafts.iter().enumerate() {
            let key = format!("pages/recovered/{index}");
            content.push(kit::notice(
                format!("{key}/box"),
                kit::spaced(
                    kit::column(
                        &key,
                        [
                            kit::label(format!("{key}/label"), "Recovered comment draft"),
                            kit::wrapping(kit::text(format!("{key}/text"), draft)),
                            kit::spaced(
                                kit::row(
                                    format!("{key}/actions"),
                                    [
                                        action(
                                            format!("{key}/restore"),
                                            "Use recovered draft",
                                            Message::UseOrphanedCommentDraft(draft.clone()),
                                            !self.unavailable(),
                                            ButtonPreset::Secondary,
                                        ),
                                        action(
                                            format!("{key}/discard"),
                                            "Discard",
                                            Message::DiscardOrphanedCommentDraft(draft.clone()),
                                            true,
                                            ButtonPreset::Text,
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
        let notice = crate::editor_view::presentation_notice(&self.document, &self.document_paint);
        if !notice.is_empty() {
            content.push(kit::wrapping(kit::caption(
                "pages/document/presentation-notice",
                notice,
            )));
        }
        if !self.document_error.is_empty() {
            content.push(kit::notice(
                "pages/document/error-box",
                kit::wrapping(kit::text("pages/document/error", &self.document_error)),
                Tone::Danger,
            ));
        }
        content.push(self.document_editor());
        if !self.subpages.is_empty() {
            let mut links = vec![kit::heading("pages/subpages/title", "Subpages")];
            links.extend(self.subpages.iter().map(|page| {
                action(
                    format!("pages/subpage/{}", page.id),
                    &page.title,
                    Message::ChoosePage(page.id.clone()),
                    !self.unavailable(),
                    ButtonPreset::Text,
                )
            }));
            content.push(kit::divider("pages/subpages/rule"));
            content.push(kit::spaced(kit::column("pages/subpages", links), 4.));
        }
        let mut surface = fill(kit::padded(
            kit::container(
                "pages/document/surface",
                fill(kit::spaced(
                    kit::column("pages/document/content", content),
                    12.,
                )),
            ),
            wire::Edges {
                top: 26.,
                right: 40.,
                bottom: 18.,
                left: 22.,
            },
        ));
        if let Node::Container { max_width, .. } = &mut surface {
            *max_width = Some(crate::host::document_width(
                self.pages_pane_width,
                self.block_comments_open,
            ) as f32);
        }
        surface
    }

    fn search_panel(&self) -> Node {
        let results = if self.page_search_hits.is_empty() {
            kit::empty_state(
                "pages/search/empty-state",
                "No matching pages",
                "Try another word; titles and every block are searched.",
            )
        } else {
            kit::scroll(
                "pages/search/scroll",
                kit::spaced(
                    kit::column(
                        "pages/search/results",
                        self.page_search_hits
                            .iter()
                            .map(|hit| self.search_result(hit)),
                    ),
                    2.,
                ),
            )
        };
        let contents = fill(kit::spaced(
            kit::column(
                "pages/search/content",
                [
                    kit::centered_row(
                        "pages/search/header",
                        [
                            kit::sized(
                                kit::wrapping(kit::heading(
                                    "pages/search/query",
                                    format!("Results for {}", self.page_search_query),
                                )),
                                Some(Length::Fill),
                                None,
                            ),
                            action(
                                "pages/search/clear",
                                "Clear search",
                                Message::ClearPageSearch,
                                true,
                                ButtonPreset::Secondary,
                            ),
                        ],
                    ),
                    results,
                ],
            ),
            10.,
        ));
        let mut panel = modal(
            "pages/search/panel",
            contents,
            (self.pages_pane_width as f32 - 48.).clamp(240., 720.),
        );
        if let Node::Container { height, clip, .. } = &mut panel {
            *height = Some(Length::Fixed(400.));
            *clip = true;
        }
        overlay(
            "pages/search",
            panel,
            Message::ClearPageSearch,
            wire::AlignX::Center,
            wire::AlignY::Top,
        )
    }

    fn document_editor(&self) -> Node {
        let (document, on_document) = self
            .document
            .document("app:document".into(), Message::DocumentUpdated);
        let presentation =
            crate::editor_view::paint(self.document.state_view(), &self.document_paint);
        presentation
            .validate(self.document.state_view().text)
            .expect("invalid editor presentation");
        let editable = self.connected
            && !self.loading
            && self.host_error.is_empty()
            && !self.active_page.is_empty()
            && self.active_page == self.buffer_page;
        Node::Editor {
            key: format!("{PAGE_KEY}/document"),
            document,
            on_document,
            editable,
            placeholder: "Write with Markdown…".into(),
            width: None,
            height: None,
            min_height: None,
            max_height: None,
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(
                    crate::editor_binding::keys(
                        self.document_history.clone(),
                        self.document_menu.clone(),
                    )
                    .register(Message::DocumentCommitted, Message::DocumentTransaction),
                )),
                presentation: Some(Box::new(presentation)),
                size: Some(crate::markdown::BODY_SIZE),
                line_height: Some(wire::LineHeight::Relative(
                    crate::markdown::BODY_LINE_HEIGHT,
                )),
                wrapping: Some(wire::Wrapping::Word),
                ..Default::default()
            }),
        }
    }

    fn comments_layer(&self) -> Node {
        use wire::FloatOp::{Add, Geometry, Multiply, Number, Subtract};
        Node::Float {
            key: "pages/comments/anchor".into(),
            x: wire::FloatExpression {
                ops: vec![
                    Geometry(4),
                    Geometry(6),
                    Add,
                    Geometry(0),
                    Subtract,
                    Geometry(2),
                    Subtract,
                    Number(crate::host::comments_right_anchor(self.pages_pane_width)),
                    Multiply,
                    Number(crate::host::comments_left_inset(self.pages_pane_width)),
                    Add,
                ],
            },
            y: wire::FloatExpression {
                ops: vec![Number(crate::host::comment_card_offset(
                    self.pages_pane_width,
                    self.comment_anchor_y,
                    self.pages_viewport_height,
                ))],
            },
            scale: 1.,
            shadow: Default::default(),
            radius: None,
            content: Box::new(measured(
                "pages/comments/size",
                self.comments_card(),
                Message::CommentsCardMeasured,
            )),
        }
    }

    fn comments_card(&self) -> Node {
        let disabled = self.unavailable() || self.threads_loading;
        let mut header = vec![kit::sized(
            kit::wrapping(kit::strong(
                "pages/comments/title",
                crate::host::comment_scope_label(
                    &self.blocks,
                    &self.scope_target,
                    &self.active_page,
                    self.thread_total,
                ),
            )),
            Some(Length::Fill),
            None,
        )];
        if !self.scope_pinned && !self.scope_target.is_empty() {
            header.push(action(
                "pages/comments/widen",
                "All comments on this page",
                Message::WidenCommentScope,
                true,
                ButtonPreset::Text,
            ));
        }
        header.push(action(
            "pages/comments/close",
            "Close",
            Message::CloseBlockComments,
            true,
            ButtonPreset::Subtle,
        ));
        let mut threads = Vec::new();
        if self.threads_loading {
            threads.push(kit::secondary("pages/comments/loading", "Loading comments…"));
        }
        let groups = crate::host::scope_groups(
            self.comment_rows.clone(),
            &self.scope_target,
            &self.active_page,
        );
        let resolved = crate::host::scope_resolved(self.comment_rows.clone(), &self.scope_target);
        if groups.is_empty() && resolved.is_empty() {
            threads.push(kit::wrapping(kit::secondary(
                "pages/comments/empty",
                crate::host::empty_scope_label(&self.scope_target),
            )));
        }
        for group in groups {
            let block_anchor = self.scope_target.is_empty()
                && group.target != self.active_page
                && !group.anchor.is_empty();
            if block_anchor {
                threads.push(named(
                    action(
                        format!("pages/comments/scope/{}", group.target),
                        &group.anchor,
                        Message::NarrowCommentScope(group.target.clone()),
                        !disabled,
                        ButtonPreset::Text,
                    ),
                    "Comments on this block",
                ));
            }
            threads.extend(
                group
                    .threads
                    .iter()
                    .map(|thread| self.comment_thread(thread)),
            );
        }
        if !resolved.is_empty() {
            threads.push(named(
                action(
                    "pages/comments/resolved",
                    crate::host::resolved_label(&resolved),
                    Message::ToggleResolvedComments,
                    true,
                    ButtonPreset::Text,
                ),
                "Resolved threads",
            ));
            if self.resolved_open {
                threads.extend(resolved.iter().map(|row| self.comment_thread(&row.thread)));
            }
        }
        let body = fill(kit::spaced(
            kit::column(
                "pages/comments/content",
                [
                    kit::spaced(kit::centered_row("pages/comments/header", header), 4.),
                    kit::divider("pages/comments/rule"),
                    kit::scroll(
                        "pages/comments/scroll",
                        kit::spaced(kit::column("pages/comments/threads", threads), 6.),
                    ),
                    kit::wrapping(kit::caption(
                        "pages/comments/hint",
                        crate::host::compose_hint_of(
                            &self.blocks,
                            &self.scope_target,
                            &self.active_page,
                        ),
                    )),
                    kit::spaced(
                        kit::row(
                            "pages/comments/compose",
                            [
                                input(
                                    format!("{PAGE_KEY}/page-comment({})", self.active_page),
                                    "Start a thread…",
                                    &self.block_comment_draft,
                                    Message::CommentDraftChanged,
                                    Some(Message::PostBlockCommentSubmit),
                                    disabled,
                                ),
                                action(
                                    "pages/comments/submit",
                                    "Post",
                                    Message::PostBlockCommentSubmit,
                                    !disabled && !self.block_comment_draft.trim().is_empty(),
                                    ButtonPreset::Primary,
                                ),
                            ],
                        ),
                        6.,
                    ),
                ],
            ),
            8.,
        ));
        let limit =
            crate::host::comment_card_height(self.comment_anchor_y, self.pages_viewport_height)
                as f32;
        // The measured key is the plain frame: the card's border lives inside
        // it, so the frame's width is the width the pane arithmetic used.
        let mut card = kit::card("PagesView/root/pages/comments-card/card", body);
        if let Node::Container {
            clip,
            width,
            height,
            padding,
            ..
        } = &mut card
        {
            *clip = true;
            *width = Some(Length::Fill);
            *height = Some(Length::Fill);
            *padding = Some(wire::Edges::all(12.));
        }
        kit::sized(
            kit::container("PagesView/root/pages/comments-card", card),
            Some(Length::Fixed(
                crate::host::comments_card_width(self.pages_pane_width) as f32,
            )),
            Some(Length::Fixed(limit)),
        )
    }
}
