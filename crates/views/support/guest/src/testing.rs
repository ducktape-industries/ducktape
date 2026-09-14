//! Helpers for a guest's own tests: build events the host would send, and
//! read the tree a frame carries.

use crate::wire::{ButtonContent, Event, Frame, Node};

/// Every text the tree shows, depth first: text nodes, button labels, and
/// the value or placeholder of an input or editor.
pub fn texts(frame: &Frame) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(root) = &frame.root {
        collect_texts(root, &mut out);
    }
    out
}

fn collect_texts(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::Container { content, .. }
        | Node::Sensor { child: content, .. }
        | Node::Pin { content, .. }
        | Node::Float { content, .. }
        | Node::Responsive { content, .. }
        | Node::Lazy { content, .. }
        | Node::ResizeHandle { content, .. }
        | Node::MouseArea { content, .. }
        | Node::Scroll { content, .. } => collect_texts(content, out),
        Node::Linear { children, .. }
        | Node::Grid { children, .. }
        | Node::Stack { children, .. }
        | Node::Hover { children, .. }
        | Node::Tooltip { children, .. }
        | Node::Overlay { children, .. }
        | Node::KeyedColumn { children, .. }
        | Node::Flex { children, .. }
        | Node::When { children, .. } => {
            children.iter().for_each(|child| collect_texts(child, out))
        }
        Node::RichText { spans, .. } => {
            out.push(spans.iter().map(|span| span.content.as_str()).collect())
        }
        Node::Text { content, .. } => out.push(content.clone()),
        Node::Input {
            value: text,
            placeholder,
            ..
        } => out.push(if text.is_empty() {
            placeholder.clone()
        } else {
            text.clone()
        }),
        // Editor text belongs to its document transfer, not the display tree.
        Node::Editor {
            document,
            placeholder,
            ..
        } => {
            if document.byte_len == 0 {
                out.push(placeholder.clone());
            }
        }
        Node::Button { content, .. } => match content {
            ButtonContent::Label(label) => out.push(label.clone()),
            ButtonContent::Child(child) => collect_texts(child, out),
        },
        Node::Toggle { label, .. } | Node::Radio { label, .. } => out.push(label.clone()),
        Node::ComboBox {
            options,
            selected,
            placeholder,
            ..
        } => out.push(
            selected
                .and_then(|index| options.get(index as usize))
                .cloned()
                .unwrap_or_else(|| placeholder.clone()),
        ),
        Node::PickList {
            options,
            selected,
            placeholder,
            ..
        } => out.push(match selected {
            Some(index) => options[*index as usize].clone(),
            None => placeholder.clone().unwrap_or_default(),
        }),
        Node::Space { .. }
        | Node::Rule { .. }
        | Node::Qr { .. }
        | Node::Svg { .. }
        | Node::Image { .. }
        | Node::ImageViewer { .. }
        | Node::Slider { .. }
        | Node::Progress { .. }
        | Node::Canvas { .. }
        | Node::Surface { .. } => {}
    }
}

pub fn has_text(frame: &Frame, content: &str) -> bool {
    texts(frame).iter().any(|text| text == content)
}

/// The node under `key` (`App/content/count`), if the tree has one.
pub fn find<'a>(frame: &'a Frame, key: &str) -> Option<&'a Node> {
    let root = frame.root.as_ref()?;
    find_by(root, &|node| node.key() == Some(key))
}

/// The first node `matches` accepts, depth first.
fn find_by<'a>(node: &'a Node, matches: &dyn Fn(&Node) -> bool) -> Option<&'a Node> {
    if matches(node) {
        return Some(node);
    }
    match node {
        Node::Container { content, .. }
        | Node::Sensor { child: content, .. }
        | Node::Pin { content, .. }
        | Node::Float { content, .. }
        | Node::Responsive { content, .. }
        | Node::Lazy { content, .. }
        | Node::ResizeHandle { content, .. }
        | Node::MouseArea { content, .. }
        | Node::Scroll { content, .. } => find_by(content, matches),
        Node::Linear { children, .. }
        | Node::Grid { children, .. }
        | Node::Stack { children, .. }
        | Node::Hover { children, .. }
        | Node::Tooltip { children, .. }
        | Node::Overlay { children, .. }
        | Node::KeyedColumn { children, .. }
        | Node::Flex { children, .. }
        | Node::When { children, .. } => children.iter().find_map(|child| find_by(child, matches)),

        Node::Button {
            content: ButtonContent::Child(child),
            ..
        } => find_by(child, matches),
        Node::Button { .. }
        | Node::RichText { .. }
        | Node::Text { .. }
        | Node::Qr { .. }
        | Node::Svg { .. }
        | Node::Image { .. }
        | Node::ImageViewer { .. }
        | Node::Input { .. }
        | Node::Editor { .. }
        | Node::Space { .. }
        | Node::Rule { .. }
        | Node::Toggle { .. }
        | Node::Radio { .. }
        | Node::Slider { .. }
        | Node::PickList { .. }
        | Node::ComboBox { .. }
        | Node::Progress { .. }
        | Node::Canvas { .. }
        | Node::Surface { .. } => None,
    }
}

/// The button whose key, label or accessible name is `name`.
fn button<'a>(frame: &'a Frame, name: &str) -> Option<&'a Node> {
    let root = frame.root.as_ref()?;
    find_by(root, &|node| match node {
        Node::Button {
            key,
            content,
            label,
            ..
        } => {
            key == name
                || label.as_deref() == Some(name)
                || matches!(content, ButtonContent::Label(label) if label == name)
        }
        _ => false,
    })
}

/// The input whose key or placeholder is `name`.
fn input<'a>(frame: &'a Frame, name: &str) -> Option<&'a Node> {
    let root = frame.root.as_ref()?;
    find_by(root, &|node| match node {
        Node::Input {
            key, placeholder, ..
        } => key == name || placeholder == name,
        _ => false,
    })
}

/// The events the host sends when the user presses the button with key or
/// label `name`.
pub fn press(frame: &Frame, name: &str) -> Vec<Event> {
    let Some(Node::Button { on_press, .. }) = button(frame, name) else {
        panic!("no button {name:?} in {:?}", texts(frame));
    };
    let Some(message) = on_press else {
        panic!("button {name:?} is disabled");
    };
    vec![Event::Message(*message)]
}

/// The events the host sends when the input with key or placeholder `name`
/// now reads `text`.
pub fn type_into(frame: &Frame, name: &str, text: &str) -> Vec<Event> {
    let Some(Node::Input { on_input, .. }) = input(frame, name) else {
        panic!("no input {name:?} in {:?}", texts(frame));
    };
    vec![Event::Input {
        handler: *on_input,
        text: text.to_string(),
    }]
}

/// The events the host sends when the editor with key or placeholder `name`
/// now reads `text`.
pub fn edit(frame: &Frame, name: &str, before_text: &str, text: &str) -> Vec<Event> {
    let editor = frame.root.as_ref().and_then(|root| {
        find_by(root, &|node| match node {
            Node::Editor {
                key, placeholder, ..
            } => key == name || placeholder == name,
            _ => false,
        })
    });
    let Some(Node::Editor {
        document,
        options,
        editable,
        ..
    }) = editor
    else {
        panic!("no editor {name:?}");
    };
    assert!(editable, "editor {name:?} is disabled");
    assert!(
        document.validate_text(before_text).is_ok(),
        "test must supply the actual document mirror"
    );
    let binding = options.binding.as_ref().expect("editor commit route");
    let mut after = document.clone();
    after.revision = after
        .revision
        .checked_add(1)
        .expect("editor observation revisions exhausted");
    after.text_revision += u64::from(before_text != text);
    after.byte_len = u32::try_from(text.len()).expect("editor document limit");
    after.cursor.clamp(text);
    vec![Event::EditorTransaction {
        handler: binding.on_event,
        event: crate::wire::EditorTransactionEvent::Commit {
            origin: None,
            id: crate::wire::EditorTransactionId {
                instance: 0,
                document: document.document.clone(),
                reset: document.reset,
                sequence: after.revision,
                attempt: 0,
                text_revision: document.text_revision,
                revision: document.revision,
            },
            before: document.clone(),
            after,
            patches: crate::wire::editor_document::editor_changed_span(before_text, text)
                .expect("valid editor patch"),
            kind: crate::wire::EditorEditKind::GuestPatch,
            history: crate::wire::EditorHistoryEffect::NewGroup,
            input_time_ms: 0,
        },
    }]
}

/// The events the host sends when the user submits the input with key or
/// placeholder `name`.
pub fn submit(frame: &Frame, name: &str) -> Vec<Event> {
    let Some(Node::Input { on_submit, .. }) = input(frame, name) else {
        panic!("no input {name:?} in {:?}", texts(frame));
    };
    let Some(message) = on_submit else {
        panic!("input {name:?} has no submit route");
    };
    vec![Event::Message(*message)]
}

/// The node whose key is `name`, or whose label is for a labelled control.
fn control<'a>(frame: &'a Frame, name: &str) -> Option<&'a Node> {
    let root = frame.root.as_ref()?;
    find_by(root, &|node| match node {
        Node::Toggle { key, label, .. } | Node::Radio { key, label, .. } => {
            key == name || label == name
        }
        Node::Slider { key, .. } | Node::PickList { key, .. } | Node::ComboBox { key, .. } => {
            key == name
        }
        _ => false,
    })
}

/// The events the host sends when the user flips the checkbox or toggler
/// with key or label `name` to `on`.
pub fn toggle(frame: &Frame, name: &str, on: bool) -> Vec<Event> {
    let Some(Node::Toggle { on_toggle, .. }) = control(frame, name) else {
        panic!("no checkbox or toggler {name:?} in {:?}", texts(frame));
    };
    let Some(handler) = on_toggle else {
        panic!("control {name:?} is disabled");
    };
    vec![Event::Toggle {
        handler: *handler,
        on,
    }]
}

/// The events the host sends when the user selects the radio with key or
/// label `name`.
pub fn select_radio(frame: &Frame, name: &str) -> Vec<Event> {
    let Some(Node::Radio { on_select, .. }) = control(frame, name) else {
        panic!("no radio {name:?} in {:?}", texts(frame));
    };
    vec![Event::Message(*on_select)]
}

/// The events the host sends when the user drags the slider with key
/// `name` to `value`.
pub fn slide(frame: &Frame, name: &str, value: f32) -> Vec<Event> {
    let Some(Node::Slider { on_change, .. }) = control(frame, name) else {
        panic!("no slider {name:?} in {:?}", keys(frame));
    };
    vec![Event::Slide {
        handler: *on_change,
        value,
    }]
}

/// The events the host sends when the user picks the option reading
/// `option` from the pick list with key `name`.
pub fn pick(frame: &Frame, name: &str, option: &str) -> Vec<Event> {
    let Some(
        Node::PickList {
            options, on_select, ..
        }
        | Node::ComboBox {
            options, on_select, ..
        },
    ) = control(frame, name)
    else {
        panic!("no pick list {name:?} in {:?}", keys(frame));
    };
    let Some(index) = options.iter().position(|candidate| candidate == option) else {
        panic!("no option {option:?} in {options:?}");
    };
    vec![Event::Select {
        handler: *on_select,
        index: index as u32,
    }]
}

/// The events the host sends when the sensor with key `name` measures its
/// child at `width` by `height`: a first measurement is a show, so the
/// show route hears it, and a sensor with only a resize route hears it
/// there.
pub fn measure(frame: &Frame, name: &str, width: f32, height: f32) -> Vec<Event> {
    let Some(Node::Sensor {
        on_show, on_resize, ..
    }) = find(frame, name)
    else {
        panic!("no sensor {name:?} in {:?}", keys(frame));
    };
    let Some(handler) = on_show.or(*on_resize) else {
        panic!("sensor {name:?} has no size route");
    };
    vec![Event::Size {
        handler,
        width,
        height,
    }]
}

/// The events the host sends when the sensor with key `name` leaves view.
pub fn hide(frame: &Frame, name: &str) -> Vec<Event> {
    let Some(Node::Sensor { on_hide, .. }) = find(frame, name) else {
        panic!("no sensor {name:?} in {:?}", keys(frame));
    };
    let Some(message) = on_hide else {
        panic!("sensor {name:?} has no hide route");
    };
    vec![Event::Message(*message)]
}

/// The mouse area with key `name`.
fn mouse_area<'a>(frame: &'a Frame, name: &str) -> &'a Node {
    let found = frame.root.as_ref().and_then(|root| {
        find_by(
            root,
            &|node| matches!(node, Node::MouseArea { key, .. } if key == name),
        )
    });
    match found {
        Some(node) => node,
        None => panic!("no mouse area {name:?} in {:?}", keys(frame)),
    }
}

/// The events the host sends when the pointer enters the mouse area with
/// key `name`.
pub fn hover(frame: &Frame, name: &str) -> Vec<Event> {
    let Node::MouseArea { on_enter, .. } = mouse_area(frame, name) else {
        unreachable!()
    };
    let Some(message) = on_enter else {
        panic!("mouse area {name:?} has no enter route");
    };
    vec![Event::Message(*message)]
}

/// The events the host sends when the pointer moves to (`x`, `y`) inside
/// the mouse area with key `name` — the area's own coordinates.
pub fn move_to(frame: &Frame, name: &str, x: f32, y: f32) -> Vec<Event> {
    let Node::MouseArea { on_move, .. } = mouse_area(frame, name) else {
        unreachable!()
    };
    let Some(handler) = on_move else {
        panic!("mouse area {name:?} has no move route");
    };
    vec![Event::Pointer {
        handler: *handler,
        x,
        y,
    }]
}

/// The events the host sends when the wheel turns by (`dx`, `dy`) lines
/// over the mouse area with key `name`.
pub fn scroll(frame: &Frame, name: &str, dx: f32, dy: f32) -> Vec<Event> {
    let Node::MouseArea { on_scroll, .. } = mouse_area(frame, name) else {
        unreachable!()
    };
    let Some(handler) = on_scroll else {
        panic!("mouse area {name:?} has no scroll route");
    };
    vec![Event::Scroll {
        handler: *handler,
        dx,
        dy,
        pixels: false,
    }]
}

/// Every node key in the tree, depth first.
pub fn keys(frame: &Frame) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(root) = &frame.root {
        collect_keys(root, &mut out);
    }
    out
}

fn collect_keys(node: &Node, out: &mut Vec<String>) {
    if let Some(key) = node.key() {
        out.push(key.to_string());
    }
    match node {
        Node::Container { content, .. }
        | Node::Sensor { child: content, .. }
        | Node::Pin { content, .. }
        | Node::Float { content, .. }
        | Node::Responsive { content, .. }
        | Node::Lazy { content, .. }
        | Node::ResizeHandle { content, .. }
        | Node::MouseArea { content, .. }
        | Node::Scroll { content, .. } => collect_keys(content, out),
        Node::Linear { children, .. }
        | Node::Grid { children, .. }
        | Node::Stack { children, .. }
        | Node::Hover { children, .. }
        | Node::Tooltip { children, .. }
        | Node::Overlay { children, .. }
        | Node::KeyedColumn { children, .. }
        | Node::Flex { children, .. }
        | Node::When { children, .. } => children.iter().for_each(|child| collect_keys(child, out)),

        Node::Button {
            content: ButtonContent::Child(child),
            ..
        } => collect_keys(child, out),
        Node::Button { .. }
        | Node::RichText { .. }
        | Node::Text { .. }
        | Node::Qr { .. }
        | Node::Svg { .. }
        | Node::Image { .. }
        | Node::ImageViewer { .. }
        | Node::Input { .. }
        | Node::Editor { .. }
        | Node::Space { .. }
        | Node::Rule { .. }
        | Node::Toggle { .. }
        | Node::Radio { .. }
        | Node::Slider { .. }
        | Node::PickList { .. }
        | Node::ComboBox { .. }
        | Node::Progress { .. }
        | Node::Canvas { .. }
        | Node::Surface { .. } => {}
    }
}

pub fn answer(id: u64, payload: &[u8]) -> Event {
    Event::Response {
        id,
        result: Ok(payload.to_vec()),
        done: true,
    }
}

pub fn item(id: u64, payload: &[u8]) -> Event {
    Event::Response {
        id,
        result: Ok(payload.to_vec()),
        done: false,
    }
}

pub fn refuse(id: u64, message: &str) -> Event {
    Event::Response {
        id,
        result: Err(message.to_string()),
        done: true,
    }
}
