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

fn empty_state(key: &str, title: &str, description: &str) -> Node {
    kit::empty_state(key, title, description)
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
        let title = if self.active_page_title.is_empty() {
            "Untitled"
        } else {
            &self.active_page_title
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
