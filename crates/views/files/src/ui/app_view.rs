use super::*;
use ducktape_view_guest::{kit::Tone, slots};

impl FilesView {
    pub(crate) fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
        let viewport = || {
            slots::handler::<(f32, f32), Message>(Box::new(|(width, height)| {
                Some(Message::ViewportChanged(width.into(), height.into()))
            }))
        };
        // The split runs to the edges of the content area: the screen owns
        // its own chrome, and the sensor is layout-transparent over it.
        wire::Node::Sensor {
            key: "FilesView/viewport".into(),
            reset: None,
            on_show: Some(viewport()),
            on_resize: Some(viewport()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(self.files_screen("FilesView/screen".into())),
        }
    }

    /// What the browser has to say before its rows: a draft whose network
    /// moved under it, the last answer from a write, and the rows the
    /// listing cap dropped.
    pub(super) fn notices(&self) -> Vec<wire::Node> {
        let mut children = Vec::new();
        if *self.derived_draft_parked() {
            children.push(native::notice(
                "FilesView/parked-draft",
                native::spaced(
                    native::centered_row(
                        "FilesView/parked-draft/row",
                        [
                            native::sized(
                                native::spaced(
                                    native::column(
                                        "FilesView/parked-draft/lines",
                                        [
                                            native::strong(
                                                "FilesView/draft-label",
                                                "Unsaved changes to:",
                                            ),
                                            native::wrapping(native::mono(
                                                "FilesView/draft-path",
                                                self.draft_path.clone(),
                                            )),
                                            native::caption(
                                                "FilesView/draft-hint",
                                                "Return to this file to continue editing.",
                                            ),
                                        ],
                                    ),
                                    2.,
                                ),
                                Some(wire::Length::Fill),
                                None,
                            ),
                            native::button(
                                "FilesView/discard-draft",
                                "Discard unsaved changes",
                                Some(slots::message(Message::DiscardDraft(self.draft_id))),
                                wire::ButtonPreset::Secondary,
                            ),
                        ],
                    ),
                    12.,
                ),
                Tone::Warning,
            ));
        }
        if !self.notice.is_empty() {
            children.push(native::notice(
                "FilesView/notice-box",
                native::wrapping(native::text("FilesView/notice", self.notice.clone())),
                Tone::Neutral,
            ));
        }
        if self.connected && self.omitted > 0 {
            children.push(native::notice(
                "FilesView/omissions",
                native::wrapping(native::secondary(
                    "FilesView/omission-label",
                    format!("{} rows are not shown.", self.omitted),
                )),
                Tone::Neutral,
            ));
        }
        children
    }
}
