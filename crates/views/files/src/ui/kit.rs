use super::*;
use ducktape_view_guest::slots;

impl FilesView {
    pub(super) fn confirm_delete(&self, key: String) -> wire::Node {
        let loading = *self.derived_loading();
        let cancel = if loading {
            None
        } else {
            Some(slots::message(Message::DisarmDeleteNow))
        };
        let delete = if loading {
            None
        } else {
            Some(slots::message(Message::DeleteSubmit))
        };
        let mut card = native::card(
            format!("{key}/card"),
            native::spaced(
                native::column(
                    &key,
                    [
                        native::heading(format!("{key}/title"), "Delete this object"),
                        native::wrapping(native::mono(
                            format!("{key}/target"),
                            self.delete_target.clone(),
                        )),
                        native::wrapping(native::secondary(
                            format!("{key}/warning"),
                            "The committed object is removed from duckfs for every member. Earlier snapshots keep their copies.",
                        )),
                        native::spaced(
                            native::row(
                                format!("{key}/actions"),
                                [
                                    native::spacer(),
                                    native::button(
                                        format!("{key}/cancel"),
                                        "Cancel",
                                        cancel,
                                        wire::ButtonPreset::Secondary,
                                    ),
                                    native::button(
                                        format!("{key}/delete"),
                                        "Delete object",
                                        delete,
                                        wire::ButtonPreset::Danger,
                                    ),
                                ],
                            ),
                            8.,
                        ),
                    ],
                ),
                12.,
            ),
        );
        if let wire::Node::Container { padding, width, .. } = &mut card {
            *padding = Some(wire::Edges::all(20.));
            *width = Some(wire::Length::Fixed(418.));
        }
        card
    }

    pub(super) fn disconnected(&self, key: String) -> wire::Node {
        native::empty_state(
            &key,
            "Not connected",
            "Choose a network from the sidebar to browse its files.",
        )
    }

    pub(super) fn changes_heading(&self, key: String) -> wire::Node {
        native::heading(key, "Changes vs HEAD")
    }
    pub(super) fn snapshots_heading(&self, key: String) -> wire::Node {
        native::heading(key, "Snapshots")
    }
    pub(super) fn empty_directory(&self, key: String) -> wire::Node {
        native::empty_state(
            &key,
            "Nothing here yet",
            "Add a file or folder with the name field above.",
        )
    }
}
