use ducktape_view_guest::{
    kit::{self, Tone},
    slots,
};
use wire::{ButtonPreset, Length, Node};

const PAGE_KEY: &str = "PagesView/root/pages";

fn named(mut node: Node, name: &str) -> Node {
    let Node::Button { label, .. } = &mut node else {
        unreachable!("named action")
    };
    *label = Some(name.into());
    node
}

fn action(
    key: impl Into<String>,
    label: impl Into<String>,
    message: Message,
    enabled: bool,
    preset: ButtonPreset,
) -> Node {
    kit::button(key, label, enabled.then(|| slots::message(message)), preset)
}

fn input(
    key: impl Into<String>,
    label: &str,
    value: &str,
    change: fn(String) -> Message,
    submit: Option<Message>,
    disabled: bool,
) -> Node {
    let handler = slots::handler(Box::new(move |text| Some(change(text))));
    let mut node = kit::input(
        key,
        label,
        value,
        handler,
        submit.filter(|_| !disabled).map(slots::message),
    );
    if let Node::Input { options, .. } = &mut node {
        options.disabled = disabled;
    }
    kit::sized(node, Some(Length::Fill), None)
}

fn measured(key: &str, child: Node, change: fn(f64, f64) -> Message) -> Node {
    let handler = slots::handler(Box::new(move |(width, height): (f32, f32)| {
        Some(change(width.into(), height.into()))
    }));
    Node::Sensor {
        key: key.into(),
        reset: None,
        on_show: Some(handler),
        on_resize: Some(handler),
        on_hide: None,
        anticipate: None,
        delay: None,
        child: Box::new(child),
    }
}

fn fill(node: Node) -> Node {
    kit::sized(node, Some(Length::Fill), Some(Length::Fill))
}

/// A pane header or a toolbar: one 40px centre line inset from the left edge,
/// with the hairline that separates it from what it heads.
fn header_bar(key: &str, left: f32, children: impl IntoIterator<Item = Node>) -> Node {
    kit::sized(
        kit::padded(
            kit::spaced(kit::centered_row(key, children), 8.),
            wire::Edges {
                top: 0.,
                right: 8.,
                bottom: 0.,
                left,
            },
        ),
        Some(Length::Fill),
        Some(Length::Fixed(40.)),
    )
}

fn empty_state(key: &str, title: &str, description: &str) -> Node {
    kit::empty_state(key, title, description)
}

/// One row of a dropdown: a glyph column, then the words left-aligned.
fn menu_item(key: String, glyph: &str, label: &str, message: Message, disabled: bool) -> Node {
    let content = kit::spaced(
        kit::centered_row(
            format!("{key}/row"),
            [
                kit::sized(
                    kit::nowrap(kit::text(format!("{key}/glyph"), glyph)),
                    Some(Length::Fixed(20.)),
                    None,
                ),
                kit::nowrap(kit::text(format!("{key}/label"), label)),
            ],
        ),
        8.,
    );
    let mut button = kit::button_child(
        key,
        content,
        (!disabled).then(|| slots::message(message)),
        ButtonPreset::Subtle,
    );
    if let Node::Button {
        label: accessible,
        width,
        padding,
        ..
    } = &mut button
    {
        *accessible = Some(label.into());
        *width = Some(Length::Fill);
        *padding = Some(wire::Edges {
            top: 4.,
            right: 8.,
            bottom: 4.,
            left: 8.,
        });
    }
    button
}

/// A small icon button: a glyph with an accessible name.
fn glyph(key: String, glyph: &str, label: &str, message: Message, disabled: bool) -> Node {
    let mut button = action(key, glyph, message, !disabled, ButtonPreset::Subtle);
    if let Node::Button {
        label: accessible,
        padding,
        ..
    } = &mut button
    {
        *accessible = Some(label.into());
        *padding = Some(wire::Edges {
            top: 1.,
            right: 5.,
            bottom: 1.,
            left: 5.,
        });
    }
    button
}

/// A row's worth of `node` held to the left edge: a text button reads as a
/// line of the list, not a centred banner.
fn leading(key: impl Into<String>, node: Node) -> Node {
    kit::row(key, [node, kit::spacer()])
}

/// A modal card: surface, border, the card radius, a fixed width.
fn modal(key: &str, child: Node, width: f32) -> Node {
    let mut card = kit::card(key, child);
    if let Node::Container {
        padding, width: w, ..
    } = &mut card
    {
        *padding = Some(wire::Edges::all(20.));
        *w = Some(Length::Fixed(width));
    }
    card
}

fn overlay(
    key: &str,
    card: Node,
    dismiss: Message,
    align_x: wire::AlignX,
    align_y: wire::AlignY,
) -> Node {
    Node::Overlay {
        key: key.into(),
        padding: 24.,
        backdrop: wire::Rgba([0.; 4]),
        align_x,
        align_y,
        on_dismiss: Some(slots::message(dismiss)),
        children: vec![
            Node::Space {
                width: Some(Length::Fill),
                height: Some(Length::Fill),
            },
            card,
        ],
    }
}

impl PagesView {
    fn unavailable(&self) -> bool {
        !self.connected || self.loading || self.busy || !self.host_error.is_empty()
    }

    fn delete_dialog(&self) -> Node {
        let target = crate::host::page_display_title(
            &self.pages,
            &self.page_delete_page,
            &self.active_page_title,
        );
        let title = if target.is_empty() {
            "Untitled"
        } else {
            target.as_str()
        };
        let contents = kit::spaced(
            kit::column(
                "pages/delete/content",
                [
                    kit::heading("pages/delete/title", format!("Delete {title}?")),
                    kit::wrapping(kit::secondary(
                        "pages/delete/warning",
                        "This page and its comments will be deleted.",
                    )),
                    kit::spaced(
                        kit::row(
                            "pages/delete/actions",
                            [
                                kit::spacer(),
                                action(
                                    "pages/delete/cancel",
                                    "Cancel",
                                    Message::DisarmPageDelete,
                                    true,
                                    ButtonPreset::Secondary,
                                ),
                                action(
                                    "pages/delete/confirm",
                                    "Delete page",
                                    Message::DeletePageSubmit,
                                    !self.unavailable(),
                                    ButtonPreset::Danger,
                                ),
                            ],
                        ),
                        8.,
                    ),
                ],
            ),
            12.,
        );
        overlay(
            "pages/delete",
            modal("pages/delete/card", contents, 440.),
            Message::DisarmPageDelete,
            wire::AlignX::Center,
            wire::AlignY::Center,
        )
    }
}
