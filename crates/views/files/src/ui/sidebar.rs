//! The sidebar: the places a network exposes — the shared tree, the
//! reader's own home, every other member's home the node lists — and the
//! recent snapshots, each a way into the comparison the inspector shows.

use super::*;
use ducktape_view_guest::{kit::Tone, slots};

/// How many recent snapshots the sidebar names.
const RECENTS: usize = 8;

impl FilesView {
    pub(super) fn sidebar(&self, key: String) -> wire::Node {
        let mut rows = vec![kit::inset(
            native::label(format!("{key}/places-label"), "Places"),
            wire::Edges {
                top: 10.,
                right: 10.,
                bottom: 2.,
                left: 10.,
            },
        )];
        rows.push(self.place(format!("{key}/place/shared"), "Shared", "/shared"));
        let home = self.home();
        if !home.is_empty() {
            rows.push(self.place(format!("{key}/place/home"), "My home", &home));
        }
        let others: Vec<&FsEntry> = self
            .homes
            .iter()
            .filter(|entry| entry.path != home)
            .collect();
        if !others.is_empty() {
            rows.push(kit::inset(
                native::label(format!("{key}/homes-label"), "Members"),
                wire::Edges {
                    top: 12.,
                    right: 10.,
                    bottom: 2.,
                    left: 10.,
                },
            ));
            for entry in others {
                rows.push(self.place(
                    format!("{key}/home/{}", entry.path),
                    &entry.name,
                    &entry.path,
                ));
            }
        }
        rows.push(kit::inset(
            native::label(format!("{key}/recents-label"), "Recents"),
            wire::Edges {
                top: 12.,
                right: 10.,
                bottom: 2.,
                left: 10.,
            },
        ));
        if !self.history_error.is_empty() {
            rows.push(kit::inset(
                native::wrapping(native::tone_text(
                    format!("{key}/history-error"),
                    self.history_error.clone(),
                    Tone::Danger,
                )),
                wire::Edges::all(10.),
            ));
        }
        let quiet = self.history.is_empty() && self.history_error.is_empty();
        if quiet {
            rows.push(kit::inset(
                native::caption(format!("{key}/no-recents"), "No snapshots yet."),
                wire::Edges {
                    top: 4.,
                    right: 10.,
                    bottom: 4.,
                    left: 10.,
                },
            ));
        }
        for snapshot in self.history.iter().take(RECENTS) {
            rows.push(self.recent(format!("{key}/recent/{}", snapshot.id), snapshot));
        }
        native::pane(
            key.clone(),
            native::scroll(
                format!("{key}/scroll"),
                native::padded(
                    native::spaced(native::column(format!("{key}/rows"), rows), 2.),
                    wire::Edges {
                        top: 0.,
                        right: 4.,
                        bottom: 8.,
                        left: 4.,
                    },
                ),
            ),
            wire::Length::Fixed(self.sidebar_width as f32),
        )
    }

    /// One place: the row is checked while the reader stands in it or
    /// under it.
    fn place(&self, key: String, name: &str, path: &str) -> wire::Node {
        let under = format!("{}/", path.trim_end_matches('/'));
        let here = self.nav.path == path || self.nav.path.starts_with(&under);
        let face = native::spaced(
            native::centered_row(
                format!("{key}/face"),
                [
                    native::sized(
                        native::text(format!("{key}/glyph"), "▰"),
                        Some(wire::Length::Fixed(16.)),
                        None,
                    ),
                    native::nowrap(native::text_size(
                        native::text(format!("{key}/name"), name),
                        14.,
                    )),
                ],
            ),
            6.,
        );
        let mut row = native::list_row(
            key,
            face,
            here,
            Some(slots::message(Message::Navigate(path.to_owned()))),
        );
        if let wire::Node::Button { label, padding, .. } = &mut row {
            *padding = Some(wire::Edges {
                top: 8.,
                right: 8.,
                bottom: 8.,
                left: 8.,
            });
            *label = Some(format!("Go to {path}"));
        }
        row
    }

    /// One recent snapshot: what it said, who and when; pressing it opens
    /// the comparison against the head in the inspector.
    fn recent(&self, key: String, snapshot: &crate::host::FsSnapshot) -> wire::Node {
        let message = match snapshot.message.is_empty() {
            true => snapshot.short_id.clone(),
            false => snapshot.message.clone(),
        };
        let face = native::spaced(
            native::column(
                format!("{key}/face"),
                [
                    native::nowrap(native::text(format!("{key}/message"), message)),
                    native::nowrap(native::caption(
                        format!("{key}/meta"),
                        format!(
                            "{} · {}",
                            crate::host::height_label(snapshot.height),
                            snapshot.author
                        ),
                    )),
                ],
            ),
            4.,
        );
        let mut row = native::list_row(
            key,
            face,
            self.diff_from == snapshot.id,
            Some(slots::message(Message::ShowDiffOf(snapshot.id.clone()))),
        );
        if let wire::Node::Button { label, padding, .. } = &mut row {
            *padding = Some(wire::Edges {
                top: 8.,
                right: 8.,
                bottom: 8.,
                left: 8.,
            });
            *label = Some(format!("Compare snapshot {}", snapshot.short_id));
        }
        row
    }
}
