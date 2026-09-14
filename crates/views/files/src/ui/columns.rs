//! The directory as Miller columns: the current directory on the left,
//! one more column for every folder chosen to its right, and a last column
//! that states the chosen file. A click chooses (a folder opens the next
//! column), a double-click opens (a folder becomes the directory).

use super::*;
use ducktape_view_guest::slots;

const COLUMN_WIDTH: f32 = 230.;
const ROW_HEIGHT: f32 = 26.;

impl FilesView {
    pub(super) fn columns_pane(&self, key: String) -> wire::Node {
        let mut columns = vec![self.column(
            format!("{key}/column/{}", self.nav.path),
            &self.nav.path,
            &self.listing,
            0,
        )];
        for (index, path) in self.columns.iter().enumerate() {
            let listing = self
                .column_listings
                .get(path)
                .cloned()
                .unwrap_or(Listing::Pending);
            columns.push(self.column(format!("{key}/column/{path}"), path, &listing, index + 1));
        }
        let chosen = self.selected_entry();
        if !chosen.path.is_empty() && !chosen.is_dir() {
            columns.push(self.last_column(format!("{key}/last"), &chosen));
        }
        let mut scroll = native::scroll(
            format!("{key}/scroll"),
            native::sized(
                native::spaced(native::row(format!("{key}/row"), columns), 0.),
                Some(wire::Length::Shrink),
                Some(wire::Length::Fill),
            ),
        );
        if let wire::Node::Scroll { direction, .. } = &mut scroll {
            *direction = wire::ScrollDirection::Horizontal;
        }
        native::sized(
            native::container(key, scroll),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }

    /// One column: a folder's rows. Only the first column takes the filter
    /// and the sort — the rest are the chain the reader clicked through.
    /// `index` is the column's place, 0 being the current directory.
    fn column(&self, key: String, path: &str, listing: &Listing, index: usize) -> wire::Node {
        let body = match listing {
            Listing::Pending => kit::inset(
                native::caption(format!("{key}/pending"), "Loading…"),
                wire::Edges::all(10.),
            ),
            Listing::Failed(reason) => {
                kit::error_plate(format!("{key}/failed"), reason, Message::Refresh)
            }
            Listing::Listed { .. } => self.column_rows(&key, self.rows_of_column(index)),
        };
        let name = crate::host::fs_name(path);
        let mut column = kit::filled_column(
            key.clone(),
            vec![
                kit::header_strip(
                    &format!("{key}/head"),
                    vec![native::nowrap(native::caption(format!("{key}/name"), name))],
                ),
                native::scroll(format!("{key}/scroll"), body),
            ],
        );
        if let wire::Node::Linear { width, .. } = &mut column {
            *width = Some(wire::Length::Fixed(COLUMN_WIDTH));
        }
        native::spaced(
            native::row(
                format!("{key}/framed"),
                [column, native::vertical_divider(format!("{key}/edge"))],
            ),
            0.,
        )
    }

    fn column_rows(&self, key: &str, rows: Vec<FsEntry>) -> wire::Node {
        if rows.is_empty() {
            return kit::inset(
                native::caption(format!("{key}/empty"), "Empty"),
                wire::Edges::all(10.),
            );
        }
        let (keys, children): (Vec<_>, Vec<_>) = rows
            .iter()
            .map(|entry| {
                (
                    wire::ListKey::from(browse::row_key(&entry.path)),
                    self.column_row(format!("{key}/row/{}", entry.path), entry),
                )
            })
            .unzip();
        wire::Node::KeyedColumn {
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
        }
    }

    /// One row of a column: glyph, name, and a chevron on a folder. The
    /// chosen row and every folder opened to the right read as chosen, so
    /// the chain the reader clicked through stays visible.
    fn column_row(&self, key: String, entry: &FsEntry) -> wire::Node {
        let on_chain = self.columns.contains(&entry.path);
        let chosen = entry.path == self.selected || on_chain;
        let mut cells = vec![
            native::sized(
                native::colored(
                    native::text(format!("{key}/glyph"), browse::kind_glyph(entry)),
                    native::palette().muted,
                ),
                Some(wire::Length::Fixed(16.)),
                None,
            ),
            native::sized(
                native::nowrap(native::text(format!("{key}/name"), entry.name.clone())),
                Some(wire::Length::Fill),
                None,
            ),
        ];
        if entry.is_dir() {
            cells.push(native::caption(format!("{key}/chevron"), "›"));
        }
        let face = native::spaced(native::centered_row(format!("{key}/cells"), cells), 6.);
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

    /// The last column names the chosen file. Its body is the preview when
    /// the inspector is folded away; with the inspector open the preview
    /// lives there, and this column only states the file.
    fn last_column(&self, key: String, entry: &FsEntry) -> wire::Node {
        let mut body = vec![
            native::wrapping(native::strong(format!("{key}/name"), entry.name.clone())),
            native::caption(
                format!("{key}/facts"),
                format!(
                    "{} · {}",
                    browse::kind_label(entry),
                    crate::host::size_label(entry.size)
                ),
            ),
        ];
        if !self.inspector_open {
            body.push(native::divider(format!("{key}/rule")));
            body.extend(self.preview_content(&format!("{key}/preview")));
        }
        let mut column = kit::filled_column(
            key.clone(),
            vec![
                kit::header_strip(
                    &format!("{key}/head"),
                    vec![native::caption(format!("{key}/title"), "Chosen")],
                ),
                native::scroll(
                    format!("{key}/scroll"),
                    native::padded(
                        native::spaced(native::column(format!("{key}/body"), body), 8.),
                        wire::Edges::all(12.),
                    ),
                ),
            ],
        );
        if let wire::Node::Linear { width, .. } = &mut column {
            *width = Some(wire::Length::Fixed(COLUMN_WIDTH + 60.));
        }
        column
    }
}
