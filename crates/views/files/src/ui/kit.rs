//! The pieces every pane of the browser is built from: bars, strips, grips,
//! the dialogs, and the small typed controls.

use super::*;
use ducktape_view_guest::{kit::Tone, slots};

/// A secondary button that routes `message`, or draws disabled.
pub(super) fn action(key: String, label: &str, message: Message, disabled: bool) -> wire::Node {
    native::button(
        key,
        label,
        (!disabled).then(|| slots::message(message)),
        wire::ButtonPreset::Secondary,
    )
}

/// A quiet button: a glyph or a short word on the toolbar. `name` is what
/// assistive tech and the tests call it; `face` is what is drawn.
pub(super) fn quiet(
    key: String,
    face: &str,
    name: &str,
    message: Option<Message>,
    checked: bool,
) -> wire::Node {
    let mut button = native::button(
        key,
        face,
        message.map(slots::message),
        wire::ButtonPreset::Subtle,
    );
    if let wire::Node::Button {
        label, checked: on, ..
    } = &mut button
    {
        *label = Some(name.into());
        *on = Some(checked);
    }
    button
}

/// A text or any other leaf, inset from its pane's edges.
pub(super) fn inset(node: wire::Node, padding: wire::Edges) -> wire::Node {
    let key = format!("{}/inset", node.key().unwrap_or_default());
    native::padded(native::container(key, node), padding)
}

/// A 40px strip of centred controls, inset from the pane's edges.
pub(super) fn bar(key: String, children: Vec<wire::Node>) -> wire::Node {
    native::sized(
        native::padded(
            native::spaced(native::centered_row(key, children), 6.),
            wire::Edges {
                top: 0.,
                right: 10.,
                bottom: 0.,
                left: 10.,
            },
        ),
        Some(wire::Length::Fill),
        Some(wire::Length::Fixed(40.)),
    )
}

/// The header strip over a list of rows: captions on a 26px line, then the
/// hairline that separates them from the rows.
pub(super) fn header_strip(key: &str, cells: Vec<wire::Node>) -> wire::Node {
    native::spaced(
        native::column(
            format!("{key}/box"),
            [
                native::sized(
                    native::padded(
                        native::spaced(native::centered_row(key, cells), 8.),
                        wire::Edges {
                            top: 0.,
                            right: 10.,
                            bottom: 0.,
                            left: 10.,
                        },
                    ),
                    Some(wire::Length::Fill),
                    Some(wire::Length::Fixed(26.)),
                ),
                native::divider(format!("{key}/rule")),
            ],
        ),
        0.,
    )
}

/// A grabbed divider between two panes. The hairline is what shows; the
/// 10px grip around it is what the pointer has to land on, since the handle
/// takes its size from its child.
pub(super) fn resize(key: String, route: fn(f64, f64) -> Message) -> wire::Node {
    let content = native::sized(
        native::container(
            format!("{key}/grip"),
            native::vertical_divider(format!("{key}/edge")),
        ),
        Some(wire::Length::Fixed(10.)),
        Some(wire::Length::Fill),
    );
    wire::Node::ResizeHandle {
        key,
        on_press: None,
        on_release: None,
        on_drag: Some(slots::handler::<(f64, f64), Message>(Box::new(
            move |(x, y)| Some(route(x, y)),
        ))),
        cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
        content: Box::new(content),
    }
}

/// A column that fills what it is given and stacks its children with no gap.
pub(super) fn filled_column(key: String, children: Vec<wire::Node>) -> wire::Node {
    native::sized(
        native::spaced(native::column(key, children), 0.),
        Some(wire::Length::Fill),
        Some(wire::Length::Fill),
    )
}

/// A plate that says what is wrong, with the way back beside it.
pub(super) fn error_plate(key: String, reason: &str, retry: Message) -> wire::Node {
    native::padded(
        native::spaced(
            native::column(
                key.clone(),
                [
                    native::notice(
                        format!("{key}/reason"),
                        native::wrapping(native::text(format!("{key}/text"), reason)),
                        Tone::Danger,
                    ),
                    native::spaced(
                        native::row(
                            format!("{key}/actions"),
                            [action(format!("{key}/retry"), "Try again", retry, false)],
                        ),
                        8.,
                    ),
                ],
            ),
            8.,
        ),
        wire::Edges::all(12.),
    )
}

/// A modal card over the base: the whole screen dims, and clicking outside
/// is the cancel.
pub(super) fn modal(
    key: String,
    base: wire::Node,
    card: wire::Node,
    dismiss: Message,
) -> wire::Node {
    wire::Node::Overlay {
        key,
        padding: 30.,
        backdrop: wire::Rgba([0., 0., 0., 0.45]),
        align_x: wire::AlignX::Center,
        align_y: wire::AlignY::Center,
        on_dismiss: Some(slots::message(dismiss)),
        children: vec![base, card],
    }
}

fn dialog_card(key: String, children: Vec<wire::Node>) -> wire::Node {
    let mut card = native::card(
        format!("{key}/card"),
        native::spaced(native::column(key, children), 12.),
    );
    if let wire::Node::Container {
        padding, max_width, ..
    } = &mut card
    {
        *padding = Some(wire::Edges::all(20.));
        *max_width = Some(420.);
    }
    card
}

impl FilesView {
    /// The destructive confirm: name the object, say the blast radius, offer
    /// the exit first.
    pub(super) fn confirm_delete(&self, key: String) -> wire::Node {
        let busy = self.busy_writing();
        let target = self.selected_entry();
        let subtree = target.is_dir();
        let title = match subtree {
            true => "Delete this folder and everything in it",
            false => "Delete this file",
        };
        let verb = match subtree {
            true => "Delete folder",
            false => "Delete file",
        };
        dialog_card(
            key.clone(),
            vec![
                native::heading(format!("{key}/title"), title),
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
                                (!busy).then(|| slots::message(Message::DisarmDelete)),
                                wire::ButtonPreset::Secondary,
                            ),
                            native::button(
                                format!("{key}/delete"),
                                verb,
                                (!busy).then(|| slots::message(Message::DeleteSubmit)),
                                wire::ButtonPreset::Danger,
                            ),
                        ],
                    ),
                    8.,
                ),
            ],
        )
    }

    /// The name prompt: one field, one verb, named for what it creates.
    pub(super) fn name_dialog(&self, key: String) -> wire::Node {
        let busy = self.busy_writing();
        let (title, verb, hint) = match &self.name_prompt {
            NamePrompt::NewFolder => ("New folder", "Create folder", "Folder name"),
            NamePrompt::NewFile => ("New file", "Create file", "File name"),
            NamePrompt::Rename(_) => ("Rename", "Confirm rename", "New name"),
            NamePrompt::Closed => ("", "", ""),
        };
        let name = self.name_draft.trim();
        let cannot_submit = busy || name.is_empty() || name.contains('/');
        let submit = (!cannot_submit).then(|| slots::message(Message::NameSubmit));
        let mut field = native::input(
            format!("{key}/name"),
            hint,
            &self.name_draft,
            slots::handler::<String, Message>(Box::new(|value| Some(Message::NameChanged(value)))),
            submit,
        );
        if let wire::Node::Input { options, .. } = &mut field {
            options.disabled = busy;
        }
        let mut children = vec![
            native::heading(format!("{key}/title"), title),
            native::wrapping(native::mono(
                format!("{key}/where"),
                match &self.name_prompt {
                    NamePrompt::Rename(path) => path.clone(),
                    _ => self.nav.path.clone(),
                },
            )),
            field,
        ];
        if name.contains('/') {
            children.push(native::tone_text(
                format!("{key}/slash"),
                "A name cannot contain a slash.",
                Tone::Danger,
            ));
        }
        children.push(native::spaced(
            native::row(
                format!("{key}/actions"),
                [
                    native::spacer(),
                    native::button(
                        format!("{key}/cancel"),
                        "Cancel",
                        (!busy).then(|| slots::message(Message::Prompt(NamePrompt::Closed))),
                        wire::ButtonPreset::Secondary,
                    ),
                    native::button(
                        format!("{key}/submit"),
                        verb,
                        submit,
                        wire::ButtonPreset::Primary,
                    ),
                ],
            ),
            8.,
        ));
        dialog_card(key, children)
    }
}
