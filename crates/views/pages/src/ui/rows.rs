/// One nesting step of the page tree, Notion's way: the row moves in and
/// the fold toggle sits in the step it opened.
const PAGE_TREE_STEP: f32 = 14.;
/// The fold toggle's column. A leaf keeps the empty column so every title
/// in the tree lines up whether or not its row has a toggle.
const PAGE_FOLD_SLOT: f32 = 20.;
/// The title's trailing pad: room for "…" and "+" to appear over it.
const PAGE_ROW_ACTIONS_WIDTH: f32 = 52.;
const PAGE_MENU_WIDTH: f64 = 200.;
const PAGE_MENU_INSET: f32 = 6.;
const PAGE_MENU_ITEM_HEIGHT: f32 = 28.;
/// The tallest a row menu grows before its list scrolls inside it.
const PAGE_MENU_MAX_ROWS: usize = 12;
/// A reply sits one avatar in from its opener.
const COMMENT_REPLY_INSET: f32 = 32.;
/// How far the hover bar rises above a comment's top edge.
const COMMENT_ACTIONS_RISE: f32 = 6.;
/// The resolve mark's column at an opener's right edge.
const COMMENT_MARK_WIDTH: f32 = 28.;

impl PagesView {
    /// The page tree's rows in order, with every row under a folded parent
    /// left out. The index is already depth-first, so a folded parent hides
    /// exactly the rows that follow it deeper than itself.
    fn page_tree_rows(&self) -> Vec<Node> {
        let mut rows = Vec::new();
        let mut folded_at: Option<usize> = None;
        for page in &self.pages {
            let depth = page_depth(page);
            let under_a_fold = folded_at.is_some_and(|fold| depth > fold);
            if under_a_fold {
                continue;
            }
            folded_at = None;
            rows.push(self.page_row(page));
            if self.page_folded(page) {
                folded_at = Some(depth);
            }
        }
        rows
    }

    fn page_folded(&self, page: &crate::host::PageItem) -> bool {
        page.child_count > 0 && self.folded_pages.contains(&page.id)
    }

    /// One row of the tree: the fold toggle (a parent's own small button, a
    /// leaf's empty slot), then the page itself, inset one step per depth.
    fn page_row(&self, page: &crate::host::PageItem) -> Node {
        let selected = page.id == self.active_page;
        let title = crate::host::titled(&page.title);
        let title = title.as_str();
        let key = format!("{PAGE_KEY}/page/{}", page.id);
        let available = !self.unavailable();
        let slot = match page.child_count > 0 {
            true => {
                let folded = self.page_folded(page);
                let caret = match folded {
                    true => "▸",
                    false => "▾",
                };
                let mut toggle = kit::button_child(
                    format!("{key}/fold"),
                    kit::caption(format!("{key}/caret"), caret),
                    available.then(|| slots::message(Message::TogglePageFold(page.id.clone()))),
                    ButtonPreset::Subtle,
                );
                if let Node::Button {
                    width,
                    padding,
                    label,
                    ..
                } = &mut toggle
                {
                    *width = Some(Length::Fixed(PAGE_FOLD_SLOT));
                    *padding = Some(wire::Edges {
                        top: 2.,
                        right: 0.,
                        bottom: 2.,
                        left: 0.,
                    });
                    let verb = match folded {
                        true => "Expand",
                        false => "Collapse",
                    };
                    *label = Some(format!("{verb} {title}"));
                }
                toggle
            }
            false => kit::space(Some(Length::Fixed(PAGE_FOLD_SLOT)), None),
        };
        let mut row = kit::list_row(
            key.clone(),
            kit::sized(
                kit::nowrap(kit::text(format!("{key}/title"), title)),
                Some(Length::Fill),
                None,
            ),
            selected,
            available.then(|| slots::message(Message::ChoosePage(page.id.clone()))),
        );
        if let Node::Button { label, padding, .. } = &mut row {
            *label = Some(title.to_owned());
            *padding = Some(wire::Edges {
                top: 5.,
                right: PAGE_ROW_ACTIONS_WIDTH,
                bottom: 5.,
                left: 4.,
            });
        }
        let line = kit::spaced(kit::centered_row(format!("{key}/row"), [slot, row]), 0.);
        // Notion's row: "…" and "+" appear at the right edge while the
        // pointer is on it, or while the row's own menu is open.
        let menu_open = self.page_menu_page == page.id;
        let mut wash = kit::rgba(kit::palette().surface_raised);
        wash.0[3] = 0.6;
        let mut actions = kit::spaced(
            kit::row(
                format!("{key}/actions"),
                [
                    glyph(
                        format!("{key}/more"),
                        "⋯",
                        &format!("Actions for {title}"),
                        Message::OpenPageRowMenu(page.id.clone()),
                        !available,
                    ),
                    glyph(
                        format!("{key}/add"),
                        "+",
                        &format!("Add a page inside {title}"),
                        Message::AddSubpage(page.id.clone()),
                        !available,
                    ),
                ],
            ),
            0.,
        );
        // The bar shrinks to its two glyphs so the anchor can push it right.
        if let Node::Linear { width, .. } = &mut actions {
            *width = Some(Length::Shrink);
        }
        let hover = Node::Hover {
            key: format!("{key}/hover"),
            width: Some(Length::Fill),
            height: None,
            padding: None,
            background: None,
            border: None,
            tint: Some(wash),
            radius: 4.,
            open: menu_open,
            children: vec![
                line,
                page_row_actions(format!("{key}/actions-anchor"), actions),
            ],
        };
        let area = Node::MouseArea {
            key: format!("{key}/area"),
            on_press: None,
            on_release: None,
            on_double_click: None,
            on_right_press: Some(slots::message(Message::OpenPageRowMenu(page.id.clone()))),
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: None,
            on_exit: None,
            on_move: None,
            on_press_at: None,
            on_scroll: None,
            content: Box::new(hover),
        };
        kit::padded(
            kit::container(format!("{key}/inset"), area),
            wire::Edges {
                top: 0.,
                right: 0.,
                bottom: 0.,
                left: PAGE_TREE_STEP * page_depth(page) as f32,
            },
        )
    }

    /// Where the menu's page can go: the workspace root, then every page that
    /// is neither the page itself nor inside it — the two destinations the
    /// module rejects as a cycle. Its current parent is left out: moving a
    /// page where it already is says nothing.
    fn page_move_items(
        &self,
        page: &crate::host::PageItem,
        key: &str,
        available: bool,
    ) -> Vec<Node> {
        let depth = page_depth(page);
        let inside: Vec<&str> = self
            .pages
            .iter()
            .skip_while(|row| row.id != page.id)
            .skip(1)
            .take_while(|row| page_depth(row) > depth)
            .map(|row| row.id.as_str())
            .collect();
        let mut items = vec![named(
            menu_item(
                format!("{key}/move/root"),
                "⌂",
                "Workspace",
                Message::MovePage(String::new()),
                !available || page.parent.is_empty(),
            ),
            "Move to the workspace",
        )];
        items.extend(
            self.pages
                .iter()
                .filter(|row| row.id != page.id && !inside.contains(&row.id.as_str()))
                .map(|row| {
                    // The row reads as the destination's title; the name says
                    // what pressing it does, which is what a reader hears and
                    // what a test presses.
                    named(
                        menu_item(
                            format!("{key}/move/{}", row.id),
                            "›",
                            &row.title,
                            Message::MovePage(row.id.clone()),
                            !available || row.id == page.parent,
                        ),
                        &format!("Move into {}", row.title),
                    )
                }),
        );
        items
    }

    /// The dropdown a row's "…" opens, floated at the press that opened it.
    fn page_row_menu(&self) -> Option<Node> {
        let page = self
            .pages
            .iter()
            .find(|page| page.id == self.page_menu_page)?;
        let key = format!("{PAGE_KEY}/page/{}/menu", page.id);
        let available = !self.unavailable();
        let items = match self.page_menu_moving {
            true => self.page_move_items(page, &key, available),
            false => vec![
                menu_item(
                    format!("{key}/add"),
                    "+",
                    "Add a page inside",
                    Message::AddSubpage(page.id.clone()),
                    !available,
                ),
                menu_item(
                    format!("{key}/move"),
                    "↳",
                    "Move to…",
                    Message::OfferPageMove,
                    !available,
                ),
                menu_item(
                    format!("{key}/link"),
                    "🔗",
                    "Copy link",
                    Message::CopyToClipboard(
                        crate::host::page_address(&page.id, &self.chain),
                        "Page link".into(),
                    ),
                    false,
                ),
                menu_item(
                    format!("{key}/delete"),
                    "🗑",
                    "Delete",
                    Message::ArmPageDelete(page.id.clone()),
                    !available,
                ),
            ],
        };
        // A workspace of a hundred pages must not draw a menu taller than the
        // screen: past the cap the list scrolls inside a card of that height.
        let rows = items.len().min(PAGE_MENU_MAX_ROWS) as f32;
        let size = (
            PAGE_MENU_WIDTH,
            f64::from(PAGE_MENU_INSET * 2. + rows * PAGE_MENU_ITEM_HEIGHT),
        );
        let (x, y) = crate::host::menu_origin(
            (self.page_menu_x, self.page_menu_y),
            size,
            (self.pages_viewport_width, self.pages_viewport_height),
        );
        let list = kit::spaced(kit::column(format!("{key}/items"), items), 0.);
        let mut card = kit::card(format!("{key}/card"), kit::scroll(format!("{key}/rows"), list));
        if let Node::Container {
            padding,
            width,
            max_height,
            ..
        } = &mut card
        {
            *padding = Some(wire::Edges::all(PAGE_MENU_INSET));
            *width = Some(Length::Fixed(PAGE_MENU_WIDTH as f32));
            // A ceiling, not a height: four actions draw a four-row card.
            *max_height = Some(size.1 as f32);
        }
        let shade = if kit::is_dark() { 0.5 } else { 0.16 };
        Some(Node::Float {
            key,
            x: wire::FloatExpression {
                ops: vec![wire::FloatOp::Number(x)],
            },
            y: wire::FloatExpression {
                ops: vec![wire::FloatOp::Number(y)],
            },
            scale: 1.,
            shadow: wire::Shadow {
                color: Some(wire::Rgba([0., 0., 0., shade])),
                x: Some(0.),
                y: Some(4.),
                blur: Some(16.),
            },
            radius: Some([8.; 4]),
            content: Box::new(card),
        })
    }

    fn search_result(&self, hit: &crate::host::PageSearchHit) -> Node {
        let key = format!("{PAGE_KEY}/search/{}/{}", hit.page_id, hit.block_id);
        let content = kit::spaced(
            kit::column(
                format!("{key}/content"),
                [
                    kit::spaced(
                        kit::centered_row(
                            format!("{key}/head"),
                            [
                                kit::sized(
                                    kit::nowrap(kit::weighted(
                                        kit::text(format!("{key}/title"), &hit.page_title),
                                        wire::Weight::Medium,
                                    )),
                                    Some(Length::Fill),
                                    None,
                                ),
                                kit::badge(format!("{key}/kind"), &hit.kind, Tone::Neutral),
                            ],
                        ),
                        6.,
                    ),
                    kit::wrapping(kit::colored(
                        kit::text_size(
                            kit::text(format!("{key}/excerpt"), &hit.text),
                            kit::type_scale::SECONDARY as f32,
                        ),
                        kit::palette().muted,
                    )),
                ],
            ),
            4.,
        );
        kit::padded(
            kit::list_row(
                key,
                content,
                false,
                (!self.unavailable()).then(|| {
                    slots::message(Message::OpenPageSearchHit(
                        hit.page_id.clone(),
                        hit.block_id.clone(),
                    ))
                }),
            ),
            wire::Edges::all(8.),
        )
    }

    /// One thread, Notion's card: the opener's line (avatar, name, the
    /// resolve mark), its words with Edit and Delete under the pointer, the
    /// replies in the same shape one step in, and a reply box that is always
    /// there to be written in while the thread is open.
    fn comment_thread(&self, thread: &crate::host::PageCommentThread) -> Node {
        let key = format!("{PAGE_KEY}/thread/{}", thread.id);
        let expanded = crate::host::expanded(&self.expanded_threads, &thread.id);
        let disabled = self.unavailable() || self.threads_loading;
        let resolve = glyph(
            format!("{key}/resolve"),
            if thread.resolved { "↺" } else { "✓" },
            if thread.resolved {
                "Reopen thread"
            } else {
                "Resolve thread"
            },
            Message::ResolveThreadSubmit(thread.id.clone(), !thread.resolved),
            disabled,
        );
        let mut rows = vec![self.comment_entry(
            format!("{key}/opener"),
            &thread.author,
            &crate::host::opener_id(thread),
            &crate::host::opener_text(thread),
            crate::host::opener_meta(thread),
            Some(resolve),
            disabled,
        )];
        for reply in crate::host::thread_replies(thread, expanded) {
            rows.push(kit::padded(
                kit::container(
                    format!("{key}/reply/{}/inset", reply.id),
                    self.comment_entry(
                        format!("{key}/reply/{}", reply.id),
                        &reply.author,
                        &reply.id,
                        &reply.text,
                        reply.meta.clone(),
                        None,
                        disabled,
                    ),
                ),
                wire::Edges {
                    top: 0.,
                    right: 0.,
                    bottom: 0.,
                    left: COMMENT_REPLY_INSET,
                },
            ));
        }
        let toggle = crate::host::reply_toggle_label(thread, expanded);
        if !toggle.is_empty() {
            rows.push(kit::padded(
                named(
                    action(
                        format!("{key}/replies"),
                        toggle,
                        Message::ToggleThreadReplies(thread.id.clone()),
                        !disabled,
                        ButtonPreset::Text,
                    ),
                    if expanded {
                        "Fewer replies"
                    } else {
                        "Show every reply"
                    },
                ),
                wire::Edges {
                    top: 0.,
                    right: 0.,
                    bottom: 0.,
                    left: COMMENT_REPLY_INSET,
                },
            ));
        }
        if !thread.resolved {
            rows.push(kit::padded(
                self.reply_box(&key, &thread.id, disabled),
                wire::Edges {
                    top: 2.,
                    right: 0.,
                    bottom: 0.,
                    left: COMMENT_REPLY_INSET,
                },
            ));
        }
        kit::card(
            key.clone(),
            kit::spaced(kit::column(format!("{key}/body"), rows), 6.),
        )
    }

    /// The reply box under an open thread. Every open thread has one; the
    /// draft belongs to the thread it was typed in, so a box the reader is
    /// not writing in stays empty.
    fn reply_box(&self, key: &str, thread_id: &str, disabled: bool) -> Node {
        let mine = self.reply_thread == thread_id;
        let draft = if mine { self.reply_draft.as_str() } else { "" };
        let id = thread_id.to_owned();
        let change = slots::handler::<String, Message>(Box::new(move |text| {
            Some(Message::ReplyDraftChangedIn(id.clone(), text))
        }));
        let submit =
            (!disabled).then(|| slots::message(Message::PostThreadReply(thread_id.to_owned())));
        let mut input = kit::input(
            format!("{PAGE_KEY}/thread-reply({thread_id})"),
            "Reply…",
            draft,
            change,
            submit,
        );
        if let Node::Input { options, .. } = &mut input {
            options.disabled = disabled;
        }
        let mut row = vec![kit::sized(input, Some(Length::Fill), None)];
        if mine && !self.reply_draft.trim().is_empty() {
            row.push(action(
                format!("{key}/submit"),
                "Reply",
                Message::PostThreadReply(thread_id.to_owned()),
                !disabled,
                ButtonPreset::Primary,
            ));
        }
        kit::spaced(kit::centered_row(format!("{key}/compose"), row), 6.)
    }

    /// One comment: who, then the words — or, while it is the one being
    /// rewritten, the box holding the new words. Edit and Delete show only
    /// under the pointer, at the entry's top-right; `trailing` (the
    /// opener's resolve mark) always shows. The module refuses a rewrite by
    /// anyone but the author, so the buttons ask, never gate.
    #[allow(clippy::too_many_arguments)]
    fn comment_entry(
        &self,
        key: String,
        author: &str,
        id: &str,
        text: &str,
        meta: String,
        trailing: Option<Node>,
        disabled: bool,
    ) -> Node {
        let mut who = vec![
            kit::avatar(
                format!("{key}/avatar"),
                kit::initials(author),
                Tone::Neutral,
            ),
            kit::nowrap(kit::strong(format!("{key}/author"), author)),
        ];
        if !meta.is_empty() {
            who.push(kit::nowrap(kit::caption(format!("{key}/meta"), meta)));
        }
        // The hover bar sits left of the resolve mark, never over it.
        let bar_inset = match trailing.is_some() {
            true => COMMENT_MARK_WIDTH,
            false => 0.,
        };
        who.push(kit::spacer());
        if let Some(trailing) = trailing {
            who.push(trailing);
        }
        let head = kit::spaced(kit::centered_row(format!("{key}/head"), who), 8.);
        let editing = !id.is_empty() && self.comment_edit_id == id;
        let words = if editing {
            kit::spaced(
                kit::row(
                    format!("{key}/edit"),
                    [
                        input(
                            format!("{PAGE_KEY}/comment-edit({id})"),
                            "Edit comment",
                            &self.comment_edit_draft,
                            Message::CommentEditDraftChanged,
                            Some(Message::SubmitEditComment),
                            disabled,
                        ),
                        action(
                            format!("{key}/save"),
                            "Save",
                            Message::SubmitEditComment,
                            !disabled && !self.comment_edit_draft.trim().is_empty(),
                            ButtonPreset::Primary,
                        ),
                        action(
                            format!("{key}/cancel"),
                            "Cancel",
                            Message::CancelEditComment,
                            true,
                            ButtonPreset::Text,
                        ),
                    ],
                ),
                6.,
            )
        } else {
            kit::wrapping(kit::text(format!("{key}/text"), text))
        };
        let entry = kit::spaced(kit::column(format!("{key}/entry"), [head, words]), 4.);
        let deleted_or_editing = id.is_empty() || editing;
        if deleted_or_editing {
            return entry;
        }
        let mut bar = kit::spaced(
            kit::row(
                format!("{key}/actions"),
                [
                    glyph(
                        format!("{key}/edit"),
                        "✎",
                        "Edit comment",
                        Message::BeginEditComment(id.to_owned(), text.to_owned()),
                        disabled,
                    ),
                    glyph(
                        format!("{key}/delete"),
                        "🗑",
                        "Delete comment",
                        Message::DeleteCommentSubmit(id.to_owned()),
                        disabled,
                    ),
                ],
            ),
            0.,
        );
        if let Node::Linear {
            width,
            background,
            border,
            padding,
            ..
        } = &mut bar
        {
            *width = Some(Length::Shrink);
            *background = Some(kit::rgba(kit::palette().surface_raised));
            *border = Some(wire::Border {
                color: Some(kit::rgba(kit::palette().border)),
                width: Some(1.),
                radius: Some([6.; 4]),
            });
            *padding = Some(wire::Edges::all(2.));
        }
        let mut anchor = kit::container(format!("{key}/actions-anchor"), bar);
        if let Node::Container {
            align_x,
            align_y,
            width,
            height,
            padding,
            ..
        } = &mut anchor
        {
            *align_x = Some(wire::AlignX::Right);
            *align_y = Some(wire::AlignY::Top);
            *width = Some(Length::Fill);
            *height = Some(Length::Fill);
            *padding = Some(wire::Edges {
                top: HOVER_FLOAT_LIFT - COMMENT_ACTIONS_RISE,
                right: bar_inset,
                bottom: 0.,
                left: 0.,
            });
        }
        Node::Hover {
            key: format!("{key}/hover"),
            width: Some(Length::Fill),
            height: None,
            padding: None,
            background: None,
            border: None,
            tint: None,
            radius: 0.,
            open: false,
            children: vec![entry, anchor],
        }
    }
}

/// A page's depth in the tree: the index spells it as two spaces a step.
fn page_depth(page: &crate::host::PageItem) -> usize {
    page.prefix.chars().count() / 2
}

/// The host floats a hover's second child in a layer lifted this far above
/// the row (a message bar straddles its row's top edge); the sidebar row
/// pads it back down so its actions sit on the row's own line.
const HOVER_FLOAT_LIFT: f32 = 14.;

/// The row actions' box at the right edge, over the title's trailing pad.
fn page_row_actions(key: String, actions: Node) -> Node {
    let mut anchor = kit::container(key, actions);
    if let Node::Container {
        align_x,
        align_y,
        padding,
        width,
        height,
        ..
    } = &mut anchor
    {
        *align_x = Some(wire::AlignX::Right);
        *align_y = Some(wire::AlignY::Center);
        *width = Some(Length::Fill);
        *height = Some(Length::Fill);
        *padding = Some(wire::Edges {
            top: HOVER_FLOAT_LIFT,
            right: 4.,
            bottom: 0.,
            left: 0.,
        });
    }
    anchor
}
