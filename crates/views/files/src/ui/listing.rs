//! The directory as rows: a sortable header, one row per entry with its
//! kind glyph, size and kind, the chosen row carrying its own actions, and
//! the plates for an empty, filtered-out or failed directory.

use super::*;
use ducktape_view_guest::slots;

const SIZE_WIDTH: f32 = 76.;
const KIND_WIDTH: f32 = 84.;
const ROW_HEIGHT: f32 = 28.;

/// A row's cell: a fixed-width, muted, unwrapped mono run.
fn cell(key: String, value: String, width: f32, align: wire::AlignX) -> wire::Node {
    let mut node = native::sized(
        native::nowrap(native::colored(
            native::mono(key, value),
            native::palette().muted,
        )),
        Some(wire::Length::Fixed(width)),
        None,
    );
    if let wire::Node::Text { align_x, .. } = &mut node {
        *align_x = Some(align);
    }
    node
}

impl FilesView {
    pub(super) fn list_pane(&self, key: String) -> wire::Node {
        let mut children = vec![self.list_header(format!("{key}/header"))];
        children.push(self.list_body(&key));
        kit::filled_column(key, children)
    }

    /// The column heads: each one sorts, and the one in force says which
    /// way with an arrow.
    fn list_header(&self, key: String) -> wire::Node {
        let head = |field: &str, text: &str, sort_key: SortKey, width: Option<f32>| {
            let in_force = self.sort.key == sort_key;
            let arrow = match (in_force, self.sort.ascending) {
                (false, _) => "",
                (true, true) => " ▲",
                (true, false) => " ▼",
            };
            let mut button = native::button(
                format!("{key}/{field}"),
                format!("{text}{arrow}"),
                Some(slots::message(Message::SortBy(sort_key))),
                wire::ButtonPreset::Text,
            );
            if let wire::Node::Button {
                label,
                width: w,
                padding,
                ..
            } = &mut button
            {
                *label = Some(format!("Sort by {text}"));
                *w = Some(width.map_or(wire::Length::Fill, wire::Length::Fixed));
                *padding = Some(wire::Edges::all(0.));
            }
            button
        };
        kit::header_strip(
            &key,
            vec![
                native::sized(
                    native::text(format!("{key}/glyph-space"), ""),
                    Some(wire::Length::Fixed(16.)),
                    None,
                ),
                head("name", "Name", SortKey::Name, None),
                head("size", "Size", SortKey::Size, Some(SIZE_WIDTH)),
                head("kind", "Kind", SortKey::Kind, Some(KIND_WIDTH)),
            ],
        )
    }

    fn list_body(&self, key: &str) -> wire::Node {
        match &self.listing {
            Listing::Pending => native::sized(
                native::container(
                    format!("{key}/pending-box"),
                    kit::inset(
                        native::caption(format!("{key}/pending"), "Loading…"),
                        wire::Edges::all(12.),
                    ),
                ),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            ),
            Listing::Failed(reason) => native::sized(
                native::container(
                    format!("{key}/failed-box"),
                    kit::error_plate(format!("{key}/failed"), reason, Message::Refresh),
                ),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            ),
            Listing::Listed { entries, .. } => self.listed_rows(key, entries),
        }
    }

    fn listed_rows(&self, key: &str, entries: &[FsEntry]) -> wire::Node {
        if entries.is_empty() {
            return native::sized(
                native::container(
                    format!("{key}/empty-box"),
                    native::empty_state(
                        format!("{key}/empty"),
                        "Empty folder",
                        "Nothing is committed under this path. New folder and New file add to it; a file dropped on the window uploads here.",
                    ),
                ),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            );
        }
        let rows = self.rows();
        if rows.is_empty() {
            return native::sized(
                native::container(
                    format!("{key}/no-match-box"),
                    native::empty_state(
                        format!("{key}/no-match"),
                        "No names match",
                        "Clear the filter to see every entry.",
                    ),
                ),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            );
        }
        let (keys, children): (Vec<_>, Vec<_>) = rows
            .iter()
            .map(|entry| {
                (
                    wire::ListKey::from(browse::row_key(&entry.path)),
                    self.list_row(format!("{key}/row/{}", entry.path), entry),
                )
            })
            .unzip();
        let mut column = wire::Node::KeyedColumn {
            key: format!("{key}/rows"),
            keys: Some(keys),
            children,
            background: None,
            border: None,
            spacing: None,
            padding: Some(wire::Edges {
                top: 4.,
                right: 2.,
                bottom: 6.,
                left: 2.,
            }),
            width: Some(wire::Length::Fill),
            height: None,
            max_width: None,
            align: None,
            virtual_row: Some(ROW_HEIGHT),
        };
        if self.listing.has_more() {
            // one more row, under the keyed ones: the way to the rest
            column = native::spaced(
                native::column(
                    format!("{key}/rows-and-more"),
                    [
                        column,
                        native::padded(
                            native::spaced(
                                native::centered_row(
                                    format!("{key}/more-row"),
                                    [
                                        native::caption(
                                            format!("{key}/more-note"),
                                            format!(
                                                "The first {} entries are shown.",
                                                entries.len()
                                            ),
                                        ),
                                        kit::action(
                                            format!("{key}/load-more"),
                                            "Load more",
                                            Message::LoadMore,
                                            self.loading(),
                                        ),
                                    ],
                                ),
                                8.,
                            ),
                            wire::Edges::all(10.),
                        ),
                    ],
                ),
                0.,
            );
        }
        let mut scroll = native::scroll(format!("{key}/scroll"), column);
        if let wire::Node::Scroll { virtual_rows, .. } = &mut scroll {
            *virtual_rows = !self.listing.has_more();
        }
        scroll
    }

    /// One entry. A click chooses it; a double-click opens it. The chosen
    /// row carries its Rename and Delete beside its cells.
    fn list_row(&self, key: String, entry: &FsEntry) -> wire::Node {
        let chosen = entry.path == self.selected;
        let name = native::nowrap(native::text(format!("{key}/name"), entry.name.clone()));
        let name = match entry.is_dir() {
            true => native::weighted(name, wire::Weight::Medium),
            false => name,
        };
        let size = match entry.is_dir() {
            true => "—".into(),
            false => crate::host::size_label(entry.size),
        };
        let mut cells = vec![
            native::sized(
                native::colored(
                    native::text(format!("{key}/glyph"), browse::kind_glyph(entry)),
                    native::palette().muted,
                ),
                Some(wire::Length::Fixed(16.)),
                None,
            ),
            native::sized(name, Some(wire::Length::Fill), None),
        ];
        if chosen {
            cells.push(self.row_actions(format!("{key}/actions"), entry));
        }
        cells.push(cell(
            format!("{key}/size"),
            size,
            SIZE_WIDTH,
            wire::AlignX::Right,
        ));
        cells.push(cell(
            format!("{key}/kind"),
            browse::kind_label(entry),
            KIND_WIDTH,
            wire::AlignX::Left,
        ));
        let face = native::spaced(native::centered_row(format!("{key}/cells"), cells), 8.);
        let mut button = native::list_row(
            format!("{key}/select"),
            face,
            chosen,
            Some(slots::message(Message::Select(entry.path.clone()))),
        );
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some(match entry.is_dir() {
                true => format!("Folder {}", entry.name),
                false => format!("File {}", entry.name),
            });
        }
        wire::Node::MouseArea {
            key,
            on_press: None,
            on_release: None,
            on_double_click: Some(slots::message(Message::Open(entry.path.clone()))),
            on_right_press: Some(slots::message(Message::Select(entry.path.clone()))),
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: None,
            on_exit: None,
            on_move: None,
            on_press_at: None,
            on_scroll: None,
            content: Box::new(button),
        }
    }

    /// Open (a folder), Rename and Delete, on the chosen row only; the
    /// toolbar's verbs act on the directory, these on the entry.
    pub(super) fn row_actions(&self, key: String, entry: &FsEntry) -> wire::Node {
        let busy = self.loading();
        let refused = !crate::host::write_refusal(&crate::host::fs_parent(&entry.path)).is_empty();
        let cannot = busy || refused;
        let mut actions = Vec::new();
        if entry.is_dir() {
            actions.push(kit::quiet(
                format!("{key}/open"),
                "Open",
                &format!("Open {}", entry.name),
                Some(Message::Open(entry.path.clone())),
                false,
            ));
        }
        actions.push(kit::quiet(
            format!("{key}/rename"),
            "Rename",
            &format!("Rename {}", entry.name),
            (!cannot).then(|| Message::Prompt(NamePrompt::Rename(entry.path.clone()))),
            false,
        ));
        actions.push(kit::quiet(
            format!("{key}/delete"),
            "Delete",
            &format!("Delete {}", entry.name),
            (!cannot).then(|| Message::ArmDelete(entry.path.clone())),
            false,
        ));
        native::spaced(native::centered_row(key, actions), 2.)
    }
}
