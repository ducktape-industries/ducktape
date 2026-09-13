use super::*;
use ducktape_view_guest::{kit::Tone, slots};

fn object_cells(key: &str, name: wire::Node, size: String, object: String) -> wire::Node {
    let cell = |field: &str, value: String, width| {
        native::sized(
            native::nowrap(native::colored(
                native::mono(format!("{key}/{field}"), value),
                native::palette().muted,
            )),
            Some(width),
            None,
        )
    };
    native::spaced(
        native::centered_row(
            key,
            [
                native::sized(name, Some(wire::Length::Fill), None),
                cell("size", size, wire::Length::Fixed(72.)),
                cell("object", object, wire::Length::Fixed(92.)),
            ],
        ),
        12.,
    )
}

impl FilesView {
    pub(super) fn breadcrumb(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
    ) -> wire::Node {
        let mut root = native::button(
            format!("{key}/root"),
            "duckfs",
            Some(slots::message(open("/".into()))),
            wire::ButtonPreset::Text,
        );
        if let wire::Node::Button { label, .. } = &mut root {
            *label = Some("Go to the duckfs root".into());
        }
        native::spaced(
            native::centered_row(
                &key,
                [
                    root,
                    native::sized(
                        native::nowrap(native::mono(format!("{key}/path"), self.path.clone())),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    native::secondary(
                        format!("{key}/count"),
                        crate::host::fs_counts_summary(self.connected, self.listed, &self.entries),
                    ),
                ],
            ),
            8.,
        )
    }

    pub(super) fn folder_row(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
        entry: crate::host::FsEntry,
    ) -> wire::Node {
        let name = native::nowrap(native::text(
            format!("{key}/name"),
            format!("{}/", entry.name),
        ));
        let mut button = native::list_row(
            key,
            name,
            entry.path == self.path,
            Some(slots::message(open(entry.path))),
        );
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some("Open directory".into());
        }
        button
    }

    pub(super) fn object_table_header(&self, key: String) -> wire::Node {
        let head = |field: &str, text: &str, width| {
            native::sized(
                native::label(format!("{key}/{field}"), text),
                Some(width),
                None,
            )
        };
        native::padded(
            native::column(
                format!("{key}/box"),
                [
                    native::spaced(
                        native::centered_row(
                            &key,
                            [
                                head("name", "Name", wire::Length::Fill),
                                head("size", "Size", wire::Length::Fixed(72.)),
                                head("object", "Object", wire::Length::Fixed(92.)),
                            ],
                        ),
                        12.,
                    ),
                    native::divider(format!("{key}/rule")),
                ],
            ),
            wire::Edges {
                top: 0.,
                right: 10.,
                bottom: 0.,
                left: 10.,
            },
        )
    }

    pub(super) fn object_row(
        &self,
        key: String,
        open_directory: impl Fn(String) -> Message + Clone + 'static,
        open_file: impl Fn(String) -> Message + Clone + 'static,
        entry: crate::host::FsEntry,
        selected: bool,
    ) -> wire::Node {
        let directory = entry.kind == "dir";
        let size = if directory {
            "—".into()
        } else {
            crate::host::size_label(entry.size)
        };
        let object = if entry.object.is_empty() {
            "—".into()
        } else {
            entry.object
        };
        let name = native::nowrap(native::text(
            format!("{key}/cells/name"),
            if directory {
                format!("{}/", entry.name)
            } else {
                entry.name
            },
        ));
        let name = if directory {
            native::weighted(name, wire::Weight::Medium)
        } else {
            name
        };
        let content = object_cells(&format!("{key}/cells"), name, size, object);
        let action = if directory {
            open_directory(entry.path)
        } else {
            open_file(entry.path)
        };
        let mut button = native::list_row(
            key,
            content,
            selected && !directory,
            Some(slots::message(action)),
        );
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some(
                if directory {
                    "Open directory"
                } else {
                    "Show object"
                }
                .into(),
            );
        }
        button
    }

    pub(super) fn object_panel(&self, key: String) -> wire::Node {
        let entry = &self.preview_entry;
        let directory = entry.kind == "dir";
        let object = if entry.object.is_empty() {
            "—"
        } else {
            &entry.object
        };
        let size = if directory {
            "—".into()
        } else {
            crate::host::size_label(entry.size)
        };
        let content = native::spaced(
            native::padded(
                native::column(
                    format!("{key}/facts"),
                    [
                        native::centered_row(
                            format!("{key}/head"),
                            [
                                native::sized(
                                    native::heading(format!("{key}/title"), "Object"),
                                    Some(wire::Length::Fill),
                                    None,
                                ),
                                native::badge(
                                    format!("{key}/kind"),
                                    if directory { "DIR" } else { "FILE" },
                                    Tone::Neutral,
                                ),
                            ],
                        ),
                        native::wrapping(native::strong(format!("{key}/name"), entry.name.clone())),
                        native::wrapping(native::mono(format!("{key}/path"), entry.path.clone())),
                        native::divider(format!("{key}/rule")),
                        native::spaced(
                            native::column(
                                format!("{key}/id-block"),
                                [
                                    native::label(format!("{key}/id-label"), "object id"),
                                    native::wrapping(native::mono(format!("{key}/id"), object)),
                                ],
                            ),
                            3.,
                        ),
                        native::spaced(
                            native::column(
                                format!("{key}/size-block"),
                                [
                                    native::label(format!("{key}/size-label"), "size"),
                                    native::mono(format!("{key}/size"), size),
                                ],
                            ),
                            3.,
                        ),
                    ],
                ),
                wire::Edges::all(16.),
            ),
            10.,
        );
        native::pane(
            key.clone(),
            native::scroll(format!("{key}/scroll"), content),
            wire::Length::Fixed(self.object_width as f32),
        )
    }
}
