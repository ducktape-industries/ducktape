use super::*;
use ducktape_view_guest::{kit::Tone, slots};

fn action(key: String, label: &str, message: Message, disabled: bool) -> wire::Node {
    native::button(
        key,
        label,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Secondary,
    )
}
fn resize(key: String, vertical: bool, route: fn(f64, f64) -> Message) -> wire::Node {
    let (cursor, content) = if vertical {
        (
            wire::mouse::Cursor::ResizingVertically,
            native::divider(format!("{key}/edge")),
        )
    } else {
        (
            wire::mouse::Cursor::ResizingHorizontally,
            native::vertical_divider(format!("{key}/edge")),
        )
    };
    wire::Node::ResizeHandle {
        key,
        on_press: None,
        on_release: None,
        on_drag: Some(slots::handler::<(f64, f64), Message>(Box::new(
            move |(x, y)| Some(route(x, y)),
        ))),
        cursor: Some(cursor),
        content: Box::new(content),
    }
}
/// A card that holds panes edge to edge: no inset, clipped, filling.
fn frame(key: String, child: wire::Node) -> wire::Node {
    let mut card = native::card(key, child);
    if let wire::Node::Container {
        padding,
        clip,
        height,
        width,
        ..
    } = &mut card
    {
        *padding = None;
        *clip = true;
        *height = Some(wire::Length::Fill);
        *width = Some(wire::Length::Fill);
    }
    card
}
impl FilesView {
    pub(super) fn files_screen(&self, key: String) -> wire::Node {
        if !self.connected {
            return self.disconnected(format!("{key}/disconnected"));
        }
        let history = self.history_open;
        let mut children = vec![
            self.breadcrumb(format!("{key}/crumb"), Message::OpenDirAt),
            self.toolbar(&key, history),
        ];
        if !self.derived_refusal().is_empty() {
            children.push(native::notice(
                format!("{key}/refusal-box"),
                native::wrapping(native::text(
                    format!("{key}/refusal"),
                    self.derived_refusal(),
                )),
                Tone::Danger,
            ));
        }
        let center = if history {
            self.history_panel(format!("{key}/history"))
        } else {
            self.object_listing(&key)
        };
        let mut panes = vec![
            self.directory_tree(&key),
            resize(format!("{key}/tree-resize"), false, Message::TreeResized),
            center,
        ];
        if !self.preview_entry.path.is_empty() {
            panes.push(resize(
                format!("{key}/object-resize"),
                false,
                Message::ObjectResized,
            ));
            panes.push(self.object_panel(format!("{key}/object-panel")));
        }
        children.push(frame(
            format!("{key}/frame"),
            native::sized(
                native::row(format!("{key}/panes"), panes),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            ),
        ));
        let content = native::sized(
            native::spaced(native::column(key.clone(), children), 10.),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        );
        if self.delete_target.is_empty() {
            return content;
        }
        wire::Node::Overlay {
            key: format!("{key}/delete-dialog"),
            padding: 30.,
            backdrop: wire::Rgba([0., 0., 0., 0.45]),
            align_x: wire::AlignX::Center,
            align_y: wire::AlignY::Center,
            on_dismiss: Some(slots::message(Message::DisarmDeleteNow)),
            children: vec![
                content,
                self.confirm_delete(format!("{key}/confirm-delete")),
            ],
        }
    }
    fn toolbar(&self, key: &str, history: bool) -> wire::Node {
        let busy = *self.derived_loading();
        let cannot_create =
            busy || self.new_name.trim().is_empty() || !self.derived_refusal().is_empty();
        let mut input = native::input(
            format!("{key}/fs-new"),
            "new name…",
            &self.new_name,
            slots::handler::<String, Message>(Box::new(|value| {
                Some(Message::NewNameChanged(value))
            })),
            None,
        );
        if let wire::Node::Input { options, width, .. } = &mut input {
            options.label = "New entry name".into();
            options.disabled = busy;
            *width = Some(wire::Length::Fixed(180.));
        }
        let mut children = vec![
            action(
                format!("{key}/parent"),
                "Parent directory",
                Message::OpenDirAt(crate::host::fs_parent(&self.path)),
                busy || self.path == "/",
            ),
            input,
            action(
                format!("{key}/mkdir"),
                "+ Folder",
                Message::MkdirSubmit,
                cannot_create,
            ),
            action(
                format!("{key}/new-file"),
                "+ File",
                Message::NewFileSubmit,
                cannot_create,
            ),
        ];
        if busy {
            children.push(native::secondary(format!("{key}/loading"), "Loading…"));
        }
        children.push(native::spacer());
        if !self.preview_path.is_empty() {
            children.push(native::button(
                format!("{key}/delete"),
                "Delete object",
                (!(busy || !self.delete_target.is_empty()))
                    .then(|| slots::message(Message::ArmDeleteAt(self.preview_path.clone()))),
                wire::ButtonPreset::Danger,
            ));
        }
        let mut toggle = native::button(
            format!("{key}/history-toggle"),
            "History",
            Some(slots::message(Message::ToggleHistory)),
            wire::ButtonPreset::Subtle,
        );
        if let wire::Node::Button {
            expanded, checked, ..
        } = &mut toggle
        {
            *expanded = Some(history);
            *checked = Some(history);
        }
        children.push(toggle);
        native::spaced(native::centered_row(format!("{key}/toolbar"), children), 6.)
    }
    fn directory_tree(&self, key: &str) -> wire::Node {
        let mut children = vec![native::padded(
            native::spaced(
                native::column(
                    format!("{key}/tree-head"),
                    [
                        native::heading(format!("{key}/tree-title"), "duckfs"),
                        native::caption(
                            format!("{key}/tree-note"),
                            "content-addressed · replicated",
                        ),
                    ],
                ),
                2.,
            ),
            wire::Edges {
                top: 12.,
                right: 10.,
                bottom: 8.,
                left: 10.,
            },
        )];
        if self.listed {
            if self.directories.is_empty() && self.omitted == 0 {
                children.push(native::padded(
                    native::column(
                        format!("{key}/no-folders-box"),
                        [native::secondary(format!("{key}/no-folders"), "No folders here.")],
                    ),
                    wire::Edges::all(10.),
                ));
            }
            for entry in &self.directories {
                children.push(self.folder_row(
                    format!("{key}/directory/{}", entry.path),
                    Message::OpenDirAt,
                    entry.clone(),
                ));
            }
        }
        native::pane(
            format!("{key}/tree-pane"),
            native::scroll(
                format!("{key}/directories"),
                native::padded(
                    native::spaced(native::column(format!("{key}/directory-rows"), children), 1.),
                    wire::Edges {
                        top: 0.,
                        right: 6.,
                        bottom: 6.,
                        left: 6.,
                    },
                ),
            ),
            wire::Length::Fixed(self.tree_width as f32),
        )
    }
    fn object_listing(&self, key: &str) -> wire::Node {
        let mut children = vec![native::padded(
            native::column(
                format!("{key}/table-head-box"),
                [self.object_table_header(format!("{key}/table-header"))],
            ),
            wire::Edges {
                top: 10.,
                right: 0.,
                bottom: 0.,
                left: 0.,
            },
        )];
        if self.listed && self.entries.is_empty() && self.omitted == 0 {
            children.push(self.empty_directory(format!("{key}/empty")));
        }
        if self.listed && !self.entries.is_empty() {
            let (keys, rows) = self
                .entries
                .iter()
                .map(|entry| {
                    (
                        wire::ListKey::from(entry.key),
                        self.object_row(
                            format!("{key}/object/{}", entry.key),
                            Message::OpenDirAt,
                            Message::OpenFileAt,
                            entry.clone(),
                            entry.path == self.preview_path,
                        ),
                    )
                })
                .unzip();
            let rows = wire::Node::KeyedColumn {
                key: format!("{key}/objects"),
                keys: Some(keys),
                children: rows,
                background: None,
                border: None,
                spacing: None,
                padding: Some(wire::Edges {
                    top: 4.,
                    right: 6.,
                    bottom: 6.,
                    left: 6.,
                }),
                width: Some(wire::Length::Fill),
                height: None,
                max_width: None,
                align: None,
                virtual_row: Some(39.),
            };
            let mut scroll = native::scroll(format!("{key}/object-scroll"), rows);
            if let wire::Node::Scroll { virtual_rows, .. } = &mut scroll {
                *virtual_rows = true;
            }
            children.push(scroll);
        }
        if !self.preview_path.is_empty() {
            children.push(resize(
                format!("{key}/preview-resize"),
                true,
                Message::PreviewResized,
            ));
            children.push(self.preview_panel(format!("{key}/preview-pane")));
        }
        native::sized(
            native::column(format!("{key}/browser"), children),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }
    fn history_panel(&self, key: String) -> wire::Node {
        let mut children = Vec::new();
        if self.diff_from.is_empty() {
            if !self.history.is_empty() {
                children.push(self.snapshots_heading(format!("{key}/title")));
            }
            if self.history.is_empty() && self.omitted == 0 {
                children.push(native::secondary(format!("{key}/empty"), "No snapshots yet."));
            }
            for snapshot in &self.history {
                let scope = format!("{key}/{}", snapshot.id);
                children.push(native::card(
                    scope.clone(),
                    native::spaced(
                        native::column(
                            format!("{scope}/body"),
                            [
                                native::spaced(
                                    native::centered_row(
                                        format!("{scope}/details"),
                                        [
                                            native::nowrap(native::mono(
                                                format!("{scope}/id"),
                                                &snapshot.short_id,
                                            )),
                                            native::badge(
                                                format!("{scope}/height"),
                                                crate::host::height_label(snapshot.height),
                                                Tone::Neutral,
                                            ),
                                            native::sized(
                                                native::nowrap(native::secondary(
                                                    format!("{scope}/author"),
                                                    &snapshot.author,
                                                )),
                                                Some(wire::Length::Fill),
                                                None,
                                            ),
                                            native::button(
                                                format!("{scope}/diff"),
                                                "Diff",
                                                Some(slots::message(Message::ShowDiffOf(
                                                    snapshot.id.clone(),
                                                ))),
                                                wire::ButtonPreset::Subtle,
                                            ),
                                        ],
                                    ),
                                    8.,
                                ),
                                native::wrapping(native::text(
                                    format!("{scope}/message"),
                                    &snapshot.message,
                                )),
                            ],
                        ),
                        6.,
                    ),
                ));
            }
        } else {
            children.push(native::centered_row(
                format!("{key}/heading"),
                [
                    native::sized(
                        self.changes_heading(format!("{key}/title")),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    action(format!("{key}/back"), "Back", Message::CloseDiffNow, false),
                ],
            ));
            if self.diff.is_empty() && self.diff_omitted == 0 {
                children.push(native::secondary(format!("{key}/empty"), "No differences."));
            }
            for entry in &self.diff {
                let tone = match entry.kind.as_str() {
                    "added" | "A" => Tone::Success,
                    "removed" | "deleted" | "D" => Tone::Danger,
                    _ => Tone::Warning,
                };
                children.push(native::spaced(
                    native::centered_row(
                        format!("{key}/{}", entry.path),
                        [
                            native::badge(format!("{key}/{}/kind", entry.path), &entry.kind, tone),
                            native::wrapping(native::mono(
                                format!("{key}/{}/path", entry.path),
                                &entry.path,
                            )),
                        ],
                    ),
                    8.,
                ));
            }
            if self.diff_omitted > 0 {
                children.push(native::caption(
                    format!("{key}/omitted"),
                    format!("{} changes are not shown.", self.diff_omitted),
                ));
            }
        }
        native::sized(
            native::scroll(
                key.clone(),
                native::padded(
                    native::spaced(native::column(format!("{key}/rows"), children), 8.),
                    wire::Edges::all(12.),
                ),
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }
    fn preview_panel(&self, key: String) -> wire::Node {
        let editing = *self.derived_draft_here();
        let busy = *self.derived_loading();
        let context = self.derived_edit_context();
        let mut header = vec![native::sized(
            native::nowrap(native::mono(format!("{key}/path"), &self.preview_path)),
            Some(wire::Length::Fill),
            None,
        )];
        if self.preview_truncated {
            header.push(native::badge(
                format!("{key}/truncated"),
                "first 64 KiB",
                Tone::Warning,
            ));
        }
        if self.preview_clipped && !editing {
            header.push(native::caption(
                format!("{key}/clipped"),
                "Preview shortened for display.",
            ));
        }
        let editable_preview =
            !self.preview_binary && !self.preview_picture && !editing && !self.preview_truncated;
        if editable_preview {
            header.push(action(
                format!("{key}/edit"),
                "Edit",
                Message::BeginEdit(context.clone()),
                busy || *self.derived_draft_parked()
                    || self.preview_base.is_empty()
                    || self.chain.is_empty(),
            ));
        }
        if editing {
            header.push(native::button(
                format!("{key}/cancel"),
                "Cancel",
                Some(slots::message(Message::CancelEdit(context.clone()))),
                wire::ButtonPreset::Subtle,
            ));
            header.push(native::button(
                format!("{key}/save"),
                "Save",
                (!busy).then(|| slots::message(Message::SaveEdit(context.clone()))),
                wire::ButtonPreset::Primary,
            ));
        }
        let content = if editing {
            let (document, on_document) =
                self.draft.document("app:draft".into(), Message::EditDraft);
            wire::Node::Editor {
                key: format!("{key}/fs-editor"),
                placeholder: "File contents…".into(),
                document,
                on_document,
                editable: !busy,
                options: Box::new(wire::EditorOptions {
                    binding: Some(Box::new(ducktape_view_guest::EditorBinding::<()>::plain(
                        Message::DraftTransaction,
                    ))),
                    wrapping: Some(wire::Wrapping::Word),
                    ..Default::default()
                }),
                width: None,
                height: None,
                min_height: Some(200.),
                max_height: None,
            }
        } else {
            self.preview_content(&key)
        };
        let mut pane = native::sized(
            native::column(
                key.clone(),
                [
                    native::padded(
                        native::spaced(native::centered_row(format!("{key}/toolbar"), header), 6.),
                        wire::Edges {
                            top: 8.,
                            right: 10.,
                            bottom: 8.,
                            left: 10.,
                        },
                    ),
                    native::divider(format!("{key}/rule")),
                    content,
                ],
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(self.preview_pane_height as f32)),
        );
        if let wire::Node::Linear { background, .. } = &mut pane {
            *background = Some(native::rgba(native::palette().surface));
        }
        pane
    }
    fn preview_content(&self, key: &str) -> wire::Node {
        use wire::SurfaceValue::{Bool, Str};
        let mut children = Vec::new();
        if self.preview_binary {
            children.push(native::secondary(
                format!("{key}/binary"),
                &self.preview_display_text,
            ));
        }
        if self.preview_picture {
            children.push(wire::Node::Surface {
                key: format!("{key}/fs-picture"),
                name: "picture".into(),
                args: vec![Str("files".into()), Str(self.preview_path.clone())],
                on_event: None,
            });
            children.push(native::caption(
                format!("{key}/caption"),
                crate::host::picture_caption(self.preview_width, self.preview_height),
            ));
        }
        let text_preview = !self.preview_binary && !self.preview_picture;
        if text_preview {
            let markdown = crate::host::markdown_path(&self.preview_path);
            let (name, args, on_event) = if markdown {
                (
                    "agent_markdown",
                    vec![Str(self.preview_display_text.clone()), Bool(self.dark)],
                    Some(slots::handler::<wire::SurfaceValue, Message>(Box::new(
                        |value| match value {
                            Str(link) => Some(Message::OpenLinkAt(link)),
                            _ => None,
                        },
                    ))),
                )
            } else {
                (
                    "forge_code",
                    vec![
                        Str(self.preview_display_text.clone()),
                        Str(self.preview_path.clone()),
                        Bool(self.dark),
                    ],
                    None,
                )
            };
            children.push(wire::Node::Surface {
                key: format!("{key}/document"),
                name: name.into(),
                args,
                on_event,
            });
        }
        native::scroll(
            format!("{key}/scroll"),
            native::padded(
                native::column(format!("{key}/content"), children),
                wire::Edges::all(12.),
            ),
        )
    }
}
