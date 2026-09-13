use super::*;
use ducktape_view_guest::{kit::Tone, slots};

impl FilesView {
    pub(crate) fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
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
            children.push(native::spaced(
                native::row(
                    "FilesView/omissions",
                    [
                        native::caption("FilesView/display-omitted", self.omitted.to_string()),
                        native::caption("FilesView/omission-label", "rows are not shown."),
                    ],
                ),
                4.,
            ));
        }
        let viewport = || {
            slots::handler::<(f32, f32), Message>(Box::new(|(width, height)| {
                Some(Message::ViewportChanged(width.into(), height.into()))
            }))
        };
        children.push(wire::Node::Sensor {
            key: "FilesView/viewport".into(),
            reset: None,
            on_show: Some(viewport()),
            on_resize: Some(viewport()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(self.files_screen("FilesView/screen".into())),
        });
        let mut root = native::page("FilesView/root", children);
        if let wire::Node::Linear { padding, .. } = &mut root {
            *padding = Some(wire::Edges {
                top: 16.,
                right: 20.,
                bottom: 16.,
                left: 20.,
            });
        }
        root
    }
}
