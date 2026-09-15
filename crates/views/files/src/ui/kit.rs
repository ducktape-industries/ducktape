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

/// Navigation matches the surrounding 26px controls with a 16px stroke icon.
pub(super) fn navigation(
    key: String,
    path: &str,
    name: &str,
    message: Option<Message>,
) -> wire::Node {
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="{path}"/></svg>"#
    );
    let (hash, bytes) = slots::picture(svg.as_bytes());
    let icon = wire::Node::Svg {
        key: format!("{key}/glyph"),
        inherit_button_ink: true,
        hash,
        bytes,
        label: None,
        color: None,
        hover: None,
        fit: None,
        rotation: None,
        opacity: message.is_none().then_some(0.35),
        width: Some(wire::Length::Fixed(16.)),
        height: Some(wire::Length::Fixed(16.)),
    };
    let mut button = native::button_child(
        key.clone(),
        icon,
        message.map(slots::message),
        wire::ButtonPreset::Subtle,
    );
    if let wire::Node::Button {
        label,
        width,
        height,
        padding,
        ..
    } = &mut button
    {
        *label = Some(name.into());
        *width = Some(wire::Length::Fixed(26.));
        *height = Some(wire::Length::Fixed(26.));
        *padding = Some(wire::Edges::all(5.));
    }
    wire::Node::Tooltip {
        key: format!("{key}/tooltip"),
        position: wire::TooltipPosition::Bottom,
        gap: 6.,
        padding: 6.,
        delay_ms: 400,
        snap: true,
        style: Default::default(),
        children: vec![
            button,
            native::sized(
                native::padded(
                    native::card(
                        format!("{key}/tip"),
                        native::colored(
                            native::text(format!("{key}/tip-label"), name),
                            native::palette().foreground,
                        ),
                    ),
                    wire::Edges::all(6.),
                ),
                Some(wire::Length::Shrink),
                None,
            ),
        ],
    }
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
pub(super) fn resize(key: String, route: impl Fn(f64, f64) -> Message + 'static) -> wire::Node {
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
        padding,
        width,
        max_width,
        ..
    } = &mut card
    {
        *padding = Some(wire::Edges::all(20.));
        *width = Some(wire::Length::Fixed(420.));
        *max_width = Some(420.);
    }
    card
}

impl FilesView {
    /// The destructive confirm: name the object, say the blast radius, offer
    /// the exit first.
    pub(super) fn confirm_delete(&self, key: String) -> wire::Node {
        let busy = self.busy_writing();
        let target = self.entry_at(&self.delete_target);
        let subtree = target.is_dir();
        let title = match subtree {
            true => "Delete this folder and everything in it",
            false => "Delete this file",
        };
        let verb = match subtree {
            true => "Delete folder",
            false => "Delete file",
        };
        let mut children = vec![
            native::heading(format!("{key}/title"), title),
            native::wrapping(native::mono(
                format!("{key}/target"),
                self.delete_target.clone(),
            )),
            native::wrapping(native::secondary(
                format!("{key}/warning"),
                "The committed object is removed from duckfs for every member. Earlier snapshots keep their copies.",
            )),
        ];
        children.extend(self.refusal_in_dialog(&key));
        children.push(native::spaced(
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
        ));
        dialog_card(key, children)
    }

    /// A write the node refused, said inside the dialog the reader is
    /// standing in — the base pane is under the backdrop.
    fn refusal_in_dialog(&self, key: &str) -> Option<wire::Node> {
        (!self.notice.is_empty()).then(|| {
            native::notice(
                format!("{key}/refused"),
                native::wrapping(native::text(format!("{key}/refusal"), self.notice.clone())),
                Tone::Danger,
            )
        })
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
                    NamePrompt::NewFolder | NamePrompt::NewFile | NamePrompt::Closed => {
                        self.nav.path.clone()
                    }
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
        children.extend(self.refusal_in_dialog(&key));
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
