//! The inspector: the chosen file's preview (and its editor), Get Info for
//! whatever is chosen, and — when a recent snapshot was pressed — the
//! comparison of that snapshot against the head.

use super::*;
use ducktape_view_guest::{kit::Tone, slots};

impl FilesView {
    pub(super) fn inspector(&self, key: String) -> wire::Node {
        let comparing = !self.diff_from.is_empty();
        let body = match comparing {
            true => self.comparison(format!("{key}/compare")),
            false => self.info_pane(format!("{key}/info")),
        };
        native::pane(key, body, wire::Length::Fixed(self.inspector_width as f32))
    }

    // ---- what is chosen ----

    fn info_pane(&self, key: String) -> wire::Node {
        let entry = self.chosen_for_info();
        if entry.path.is_empty() {
            return kit::filled_column(
                key.clone(),
                vec![
                    kit::bar(
                        format!("{key}/head"),
                        vec![native::heading(format!("{key}/title"), "Info")],
                    ),
                    native::divider(format!("{key}/rule")),
                    native::empty_state(
                        format!("{key}/nothing"),
                        "Nothing chosen",
                        "Click a row to see it here; double-click a folder to open it.",
                    ),
                ],
            );
        }
        let mut head = vec![native::sized(
            native::nowrap(native::heading(format!("{key}/title"), entry.name.clone())),
            Some(wire::Length::Fill),
            None,
        )];
        let previewing = !entry.is_dir();
        if previewing {
            head.extend(self.preview_actions(&key));
        }
        // A Download action belongs here, beside the preview's own verbs,
        // once the kernel opens a door that hands a duckfs file to the OS —
        // the view can read a page, not write a file on the reader's disk.
        let mut body = Vec::new();
        if previewing {
            body.push(self.preview_body(format!("{key}/preview")));
            body.push(native::divider(format!("{key}/preview-rule")));
        }
        body.push(self.get_info(format!("{key}/facts"), &entry));
        body.push(native::divider(format!("{key}/facts-rule")));
        body.push(native::padded(
            self.row_actions(format!("{key}/actions"), &entry),
            wire::Edges::all(8.),
        ));
        kit::filled_column(
            key.clone(),
            vec![
                kit::bar(format!("{key}/head"), head),
                native::divider(format!("{key}/rule")),
                native::scroll(
                    format!("{key}/scroll"),
                    native::spaced(native::column(format!("{key}/body"), body), 0.),
                ),
            ],
        )
    }

    /// The chosen entry as the inspector knows it. A choice the listing on
    /// hand does not carry — a deep link into a directory past its first
    /// page, a rename that just landed — is still a choice: its path and
    /// name are known, and what is being read says whether it is a file.
    fn chosen_for_info(&self) -> FsEntry {
        let listed = self.selected_entry();
        if !listed.path.is_empty() || self.selected.is_empty() {
            return listed;
        }
        let previewing = self.preview.path == self.selected;
        FsEntry {
            path: self.selected.clone(),
            name: crate::host::fs_name(&self.selected),
            kind: match previewing {
                true => "file".into(),
                false => "dir".into(),
            },
            size: 0,
            object: String::new(),
        }
    }

    /// Edit / Cancel / Save over the preview, and the badges that say what
    /// the read is not: whole, or shown in full.
    fn preview_actions(&self, key: &str) -> Vec<wire::Node> {
        let editing = self.draft_here();
        let busy = self.loading();
        let context = self.edit_context();
        let mut actions = Vec::new();
        if self.preview.truncated {
            actions.push(native::badge(
                format!("{key}/truncated"),
                "first 64 KiB",
                Tone::Warning,
            ));
        }
        if self.preview.clipped && !editing {
            actions.push(native::badge(
                format!("{key}/clipped"),
                "shortened",
                Tone::Neutral,
            ));
        }
        let editable = self.preview.is_read()
            && !self.preview.binary
            && !self.preview.picture
            && self.preview.error.is_empty()
            && !self.preview.truncated;
        if editable && !editing {
            actions.push(kit::action(
                format!("{key}/edit"),
                "Edit",
                Message::BeginEdit(context.clone()),
                busy || self.draft_parked()
                    || self.preview.base.is_empty()
                    || self.chain.is_empty(),
            ));
        }
        if editing {
            actions.push(native::button(
                format!("{key}/cancel"),
                "Cancel",
                Some(slots::message(Message::CancelEdit(context.clone()))),
                wire::ButtonPreset::Subtle,
            ));
            actions.push(native::button(
                format!("{key}/save"),
                "Save",
                (!busy).then(|| slots::message(Message::SaveEdit(context))),
                wire::ButtonPreset::Primary,
            ));
        }
        actions
    }

    fn preview_body(&self, key: String) -> wire::Node {
        if self.draft_here() {
            let (document, on_document) =
                self.draft.document("app:draft".into(), Message::EditDraft);
            return native::padded(
                native::card(
                    format!("{key}/editor-box"),
                    wire::Node::Editor {
                        key: format!("{key}/fs-editor"),
                        placeholder: "File contents…".into(),
                        document,
                        on_document,
                        editable: !self.loading(),
                        options: Box::new(wire::EditorOptions {
                            binding: Some(Box::new(
                                ducktape_view_guest::EditorBinding::<()>::plain(
                                    Message::DraftTransaction,
                                ),
                            )),
                            wrapping: Some(wire::Wrapping::Word),
                            ..Default::default()
                        }),
                        width: None,
                        height: None,
                        min_height: Some(240.),
                        max_height: None,
                    },
                ),
                wire::Edges::all(8.),
            );
        }
        native::padded(
            native::spaced(
                native::column(format!("{key}/content"), self.preview_content(&key)),
                6.,
            ),
            wire::Edges::all(12.),
        )
    }

    pub(super) fn preview_content(&self, key: &str) -> Vec<wire::Node> {
        use wire::SurfaceValue::{Bool, Str};
        let preview = &self.preview;
        if !preview.error.is_empty() {
            return vec![native::notice(
                format!("{key}/failed"),
                native::wrapping(native::text(format!("{key}/reason"), preview.error.clone())),
                Tone::Danger,
            )];
        }
        if !preview.is_read() {
            // the head snapshot arrives with the page: until then nothing has
            // been read, and a blank code box would read as an empty file
            return vec![native::secondary(
                format!("{key}/reading"),
                "Reading the file…",
            )];
        }
        if preview.binary {
            return vec![native::empty_state(
                format!("{key}/binary"),
                "No preview",
                preview.display_text.clone(),
            )];
        }
        if preview.picture {
            return vec![
                wire::Node::Surface {
                    key: format!("{key}/fs-picture"),
                    name: "picture".into(),
                    args: vec![Str("files".into()), Str(preview.path.clone())],
                    on_event: None,
                },
                native::caption(
                    format!("{key}/caption"),
                    crate::host::picture_caption(preview.width, preview.height),
                ),
            ];
        }
        // Binary-or-text is the wire's call; markdown-vs-code is the path's.
        let markdown = crate::host::markdown_path(&preview.path);
        let (name, args, on_event) = match markdown {
            true => (
                "agent_markdown",
                vec![Str(preview.display_text.clone()), Bool(self.dark)],
                Some(slots::handler::<wire::SurfaceValue, Message>(Box::new(
                    |value| match value {
                        Str(link) => Some(Message::OpenLinkAt(link)),
                        _ => None,
                    },
                ))),
            ),
            false => (
                "forge_code",
                vec![
                    Str(preview.display_text.clone()),
                    Str(preview.path.clone()),
                    Bool(self.dark),
                ],
                None,
            ),
        };
        let document = wire::Node::Surface {
            key: format!("{key}/document"),
            name: name.into(),
            args,
            on_event,
        };
        // The code surface scrolls within its bounds; unlike Markdown it
        // cannot measure an intrinsic height inside the inspector's scroll.
        vec![match markdown {
            true => document,
            false => native::sized(
                native::container(format!("{key}/code-box"), document),
                None,
                Some(wire::Length::Fixed(240.)),
            ),
        }]
    }

    /// Get Info: path, kind, size, object, and the snapshot that last
    /// touched it with its author — the two facts the listing wire does not
    /// carry per row, read here for the one chosen path.
    fn get_info(&self, key: String, entry: &FsEntry) -> wire::Node {
        let fact = |field: &str, name: &str, value: String| {
            native::kv(
                format!("{key}/{field}"),
                name,
                native::wrapping(native::mono(format!("{key}/{field}/value"), value)),
            )
        };
        let unlisted = entry.object.is_empty();
        let size = match (unlisted, entry.is_dir()) {
            (true, _) => "—".into(),
            (false, true) => format!("{} entries", entry.size),
            (false, false) => crate::host::size_label(entry.size),
        };
        let object = match entry.object.is_empty() {
            true => "—".into(),
            false => entry.object.clone(),
        };
        let mut rows = vec![
            fact("path", "Path", entry.path.clone()),
            fact("kind", "Kind", browse::kind_label(entry)),
            fact("size", "Size", size),
            fact("object", "Object", object),
        ];
        let (modified, author) = self.modified_line();
        rows.push(fact("modified", "Modified", modified));
        rows.push(fact("author", "Author", author));
        native::padded(
            native::spaced(native::column(key.clone(), rows), 6.),
            wire::Edges::all(12.),
        )
    }

    /// What Get Info says under Modified and Author: the snapshot the walk
    /// found, "earlier" when it fell off the walk's depth, or the wait.
    fn modified_line(&self) -> (String, String) {
        let provenance = &self.provenance;
        // the walk needs snapshots to walk: none read, or none at all, and
        // there is nothing to look in
        let no_history = !self.listing.is_pending() && self.history.is_empty();
        if !provenance.answered && no_history {
            return ("unknown".into(), "".into());
        }
        if !provenance.answered {
            return ("Looking…".into(), "".into());
        }
        if !provenance.error.is_empty() {
            return (provenance.error.clone(), "".into());
        }
        let found = !provenance.snapshot.id.is_empty();
        match found {
            true => (
                format!(
                    "{} ({})",
                    crate::host::height_label(provenance.snapshot.height),
                    provenance.snapshot.short_id
                ),
                provenance.snapshot.author.clone(),
            ),
            false => (
                format!("earlier than the last {} snapshots", provenance.searched),
                "".into(),
            ),
        }
    }

    // ---- the comparison ----

    /// The leaves that differ between a recent snapshot and the head. Each
    /// path opens its directory.
    fn comparison(&self, key: String) -> wire::Node {
        let snapshot = self
            .history
            .iter()
            .find(|snapshot| snapshot.id == self.diff_from)
            .cloned()
            .unwrap_or_default();
        let head = vec![
            native::sized(
                native::nowrap(native::heading(
                    format!("{key}/title"),
                    format!("Since {}", snapshot.short_id),
                )),
                Some(wire::Length::Fill),
                None,
            ),
            kit::action(format!("{key}/close"), "Done", Message::CloseDiff, false),
        ];
        let mut rows = vec![native::wrapping(native::secondary(
            format!("{key}/meta"),
            format!(
                "{} · {} · {}",
                crate::host::height_label(snapshot.height),
                snapshot.author,
                snapshot.message
            ),
        ))];
        if self.diff.is_empty() && self.diff_omitted == 0 {
            rows.push(native::empty_state(
                format!("{key}/empty"),
                "No differences",
                "This snapshot and the head hold the same files.",
            ));
        }
        for entry in &self.diff {
            let label = crate::host::diff_kind_label(&entry.kind);
            let tone = match label {
                "Added" => Tone::Success,
                "Removed" => Tone::Danger,
                _ => Tone::Warning,
            };
            let mut open = native::button(
                format!("{key}/{}/open", entry.path),
                entry.path.clone(),
                Some(slots::message(Message::Navigate(crate::host::fs_parent(
                    &entry.path,
                )))),
                wire::ButtonPreset::Text,
            );
            if let wire::Node::Button { label, .. } = &mut open {
                *label = Some(format!("Go to {}", entry.path));
            }
            rows.push(native::spaced(
                native::centered_row(
                    format!("{key}/{}", entry.path),
                    [
                        native::badge(format!("{key}/{}/kind", entry.path), label, tone),
                        open,
                    ],
                ),
                8.,
            ));
        }
        if self.diff_omitted > 0 {
            rows.push(native::caption(
                format!("{key}/omitted"),
                format!("{} changes are not shown.", self.diff_omitted),
            ));
        }
        kit::filled_column(
            key.clone(),
            vec![
                kit::bar(format!("{key}/head"), head),
                native::divider(format!("{key}/rule")),
                native::scroll(
                    format!("{key}/scroll"),
                    native::padded(
                        native::spaced(native::column(format!("{key}/rows"), rows), 8.),
                        wire::Edges::all(12.),
                    ),
                ),
            ],
        )
    }
}
