impl PagesView {
    fn page_button(&self, page: &crate::host::PageItem) -> Node {
        let selected = page.id == self.active_page;
        let title = if page.title.is_empty() {
            "Untitled"
        } else {
            &page.title
        };
        let key = format!("{PAGE_KEY}/page/{}", page.id);
        let depth = page.prefix.chars().count() as f32;
        let mut line = vec![kit::sized(
            kit::nowrap(kit::text(format!("{key}/title"), title)),
            Some(Length::Fill),
            None,
        )];
        if page.child_count > 0 {
            line.push(kit::caption(
                format!("{key}/children"),
                page.child_count.to_string(),
            ));
        }
        let content = kit::padded(
            kit::spaced(kit::centered_row(format!("{key}/line"), line), 6.),
            wire::Edges {
                top: 0.,
                right: 0.,
                bottom: 0.,
                left: depth * 6.,
            },
        );
        let mut node = kit::list_row(
            key,
            content,
            selected,
            (!self.unavailable()).then(|| slots::message(Message::ChoosePage(page.id.clone()))),
        );
        if let Node::Button { label, .. } = &mut node {
            *label = Some(format!("{}{title}", page.prefix));
        }
        node
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
            2.,
        );
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
        )
    }

    fn comment_thread(&self, thread: &crate::host::PageCommentThread) -> Node {
        let key = format!("{PAGE_KEY}/thread/{}", thread.id);
        let expanded = crate::host::expanded(&self.expanded_threads, &thread.id);
        let replying = self.reply_thread == thread.id && !thread.resolved;
        let disabled = self.unavailable() || self.threads_loading;
        let mut rows = vec![
            kit::spaced(
                kit::centered_row(
                    format!("{key}/header"),
                    [
                        kit::avatar(
                            format!("{key}/avatar"),
                            kit::initials(&thread.author),
                            Tone::Neutral,
                        ),
                        kit::sized(
                            kit::spaced(
                                kit::column(
                                    format!("{key}/who"),
                                    [
                                        kit::nowrap(kit::strong(
                                            format!("{key}/author"),
                                            &thread.author,
                                        )),
                                        kit::nowrap(kit::caption(
                                            format!("{key}/meta"),
                                            &thread.meta,
                                        )),
                                    ],
                                ),
                                1.,
                            ),
                            Some(Length::Fill),
                            None,
                        ),
                        action(
                            format!("{key}/resolve"),
                            if thread.resolved {
                                "Reopen"
                            } else {
                                "Resolve thread"
                            },
                            Message::ResolveThreadSubmit(thread.id.clone(), !thread.resolved),
                            !disabled,
                            ButtonPreset::Text,
                        ),
                    ],
                ),
                8.,
            ),
            self.comment_body(
                format!("{key}/opener"),
                &crate::host::opener_id(thread),
                &crate::host::opener_text(thread),
                disabled,
            ),
        ];
        for reply in crate::host::thread_replies(thread, expanded) {
            rows.push(kit::padded(
                kit::spaced(
                    kit::column(
                        format!("{key}/reply/{}", reply.id),
                        [
                            kit::caption(
                                format!("{key}/reply/{}/meta", reply.id),
                                match reply.meta.is_empty() {
                                    true => reply.author.clone(),
                                    false => format!("{} ({})", reply.author, reply.meta),
                                },
                            ),
                            self.comment_body(
                                format!("{key}/reply/{}/body", reply.id),
                                &reply.id,
                                &reply.text,
                                disabled,
                            ),
                        ],
                    ),
                    2.,
                ),
                wire::Edges {
                    top: 0.,
                    right: 0.,
                    bottom: 0.,
                    left: 36.,
                },
            ));
        }
        let toggle = crate::host::reply_toggle_label(thread, expanded);
        if !toggle.is_empty() {
            rows.push(named(
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
            ));
        }
        if replying {
            rows.push(kit::spaced(
                kit::row(
                    format!("{key}/compose"),
                    [
                        input(
                            format!("{PAGE_KEY}/thread-reply({})", thread.id),
                            "Reply…",
                            &self.reply_draft,
                            Message::ReplyDraftChanged,
                            Some(Message::PostThreadReply(thread.id.clone())),
                            disabled,
                        ),
                        action(
                            format!("{key}/submit"),
                            "Post reply",
                            Message::PostThreadReply(thread.id.clone()),
                            !disabled && !self.reply_draft.trim().is_empty(),
                            ButtonPreset::Primary,
                        ),
                    ],
                ),
                6.,
            ));
        } else if !thread.resolved {
            rows.push(action(
                format!("{key}/reply"),
                "Reply to this thread",
                Message::SelectReplyThread(thread.id.clone()),
                !disabled,
                ButtonPreset::Text,
            ));
        }
        kit::card(
            key.clone(),
            kit::spaced(kit::column(format!("{key}/body"), rows), 6.),
        )
    }

    /// One comment's words with its Edit and Delete — or, while it is the one
    /// being rewritten, the box holding the new words. The module refuses a
    /// rewrite by anyone but the author, so the buttons ask, never gate.
    fn comment_body(&self, key: String, id: &str, text: &str, disabled: bool) -> Node {
        if id.is_empty() {
            return kit::wrapping(kit::text(key, text));
        }
        if self.comment_edit_id == id {
            return kit::spaced(
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
            );
        }
        kit::spaced(
            kit::column(
                format!("{key}/said"),
                [
                    kit::wrapping(kit::text(format!("{key}/text"), text)),
                    kit::spaced(
                        kit::row(
                            format!("{key}/actions"),
                            [
                                named(
                                    action(
                                        format!("{key}/edit"),
                                        "Edit",
                                        Message::BeginEditComment(id.to_owned(), text.to_owned()),
                                        !disabled,
                                        ButtonPreset::Text,
                                    ),
                                    "Edit comment",
                                ),
                                named(
                                    action(
                                        format!("{key}/delete"),
                                        "Delete",
                                        Message::DeleteCommentSubmit(id.to_owned()),
                                        !disabled,
                                        ButtonPreset::Text,
                                    ),
                                    "Delete comment",
                                ),
                            ],
                        ),
                        4.,
                    ),
                ],
            ),
            2.,
        )
    }
}
