//! Guest controls enqueue actions into the same native editor transaction stream.
use super::{Draft, MentionChoice, editing};
use ducktape_view_guest::{
    EditorBinding, EditorDocumentUpdate, EditorStateView, EditorTransaction,
    EditorTransactionEvent, kit, slots, wire,
};
use std::rc::Rc;
use wire::keyboard::{Key, Modifiers, Named};

#[derive(Clone, Debug)]
pub struct Change {
    pub before: String,
    pub after: String,
    pub cursor: wire::EditorCursor,
    pub tag: String,
}

#[derive(Clone, Debug)]
pub enum Event<M> {
    Document(EditorDocumentUpdate),
    Transaction(EditorTransaction<M>),
    Committed(Change),
    Action(String),
}

pub enum Outcome<M> {
    Updated,
    Message(M),
    Action(String),
    Enqueue(String),
}

impl Draft {
    pub fn handle<M: 'static>(&mut self, event: Event<M>, choices: &[MentionChoice]) -> Outcome<M> {
        match event {
            Event::Document(update) => {
                update.apply(&mut self.editor);
                Outcome::Updated
            }
            Event::Transaction(transaction) => transaction
                .apply(&mut self.editor)
                .map_or(Outcome::Updated, Outcome::Message),
            Event::Committed(change) => {
                self.committed(
                    &change.before,
                    &change.after,
                    change.cursor,
                    &change.tag,
                    choices,
                );
                match change.tag.as_str() {
                    "send" | "attach" | "paste" | "copy" | "cut" | "restore" => {
                        Outcome::Action(change.tag)
                    }
                    _ => {
                        let attachment_action =
                            change.tag.starts_with("remove:") || change.tag.starts_with("retry:");
                        if attachment_action {
                            Outcome::Action(change.tag)
                        } else {
                            Outcome::Updated
                        }
                    }
                }
            }
            Event::Action(tag) => Outcome::Enqueue(tag),
        }
    }
}

fn matching_choices<'a>(choices: &'a [MentionChoice], partial: &str) -> Vec<&'a MentionChoice> {
    let needle = partial.to_lowercase();
    choices
        .iter()
        .filter(|choice| choice.label.to_lowercase().starts_with(&needle))
        .take(32)
        .collect()
}

fn key_tag(
    draft: &Draft,
    choices: &[MentionChoice],
    state: EditorStateView<'_>,
    key: &wire::keyboard::KeyState,
) -> String {
    let command = key.modifiers.control || key.modifiers.logo;
    if command {
        return match (&key.key, key.modifiers.shift) {
            (Key::Character(key), false) if key == "z" => "undo",
            (Key::Character(key), true) if key == "z" => "redo",
            (Key::Character(key), false) if key == "y" => "redo",
            (Key::Character(key), false) if key == "b" => "bold",
            (Key::Character(key), false) if key == "i" => "italic",
            (Key::Character(key), true) if key == "c" => "code",
            (Key::Character(key), true) if key == "9" => "quote",
            (Key::Character(key), false) if key == "v" => "paste",
            (Key::Character(key), false) if key == "c" => "copy",
            (Key::Character(key), false) if key == "x" => "cut",
            _ => "",
        }
        .into();
    }
    match &key.key {
        Key::Named(Named::Enter | Named::Tab) => {
            if let Some((_, partial)) = draft.query(state) {
                let choices = matching_choices(choices, &partial);
                let selected = draft.menu_index.min(choices.len().saturating_sub(1));
                if let Some(choice) = choices.get(selected) {
                    return format!("mention:{}", choice.token);
                }
            }
            if key.key == Key::Named(Named::Enter) {
                "send".into()
            } else {
                String::new()
            }
        }
        Key::Named(Named::ArrowDown) if draft.query(state).is_some() => "menu-next".into(),
        Key::Named(Named::ArrowUp) if draft.query(state).is_some() => "menu-previous".into(),
        Key::Named(Named::Escape) if draft.query(state).is_some() => "menu-dismiss".into(),
        Key::Named(Named::Backspace) => "backspace".into(),
        Key::Named(Named::Delete) => "delete".into(),
        _ => String::new(),
    }
}

pub fn editor<M: 'static>(
    draft: &Draft,
    key: &str,
    placeholder: &str,
    editable: bool,
    choices: &[MentionChoice],
    wrap: impl Fn(Event<M>) -> M + 'static,
) -> wire::Node {
    let wrap: Rc<dyn Fn(Event<M>) -> M> = Rc::new(wrap);
    let doc_wrap = wrap.clone();
    let (document, on_document) = draft
        .editor
        .document(key.into(), move |update| doc_wrap(Event::Document(update)));
    let draft = draft.clone();
    let choices = choices.to_vec();
    let bare = Modifiers::default();
    let mut claims = [
        Named::Enter,
        Named::Tab,
        Named::Backspace,
        Named::Delete,
        Named::ArrowUp,
        Named::ArrowDown,
        Named::Escape,
    ]
    .into_iter()
    .map(|key| wire::EditorKeyClaim {
        key: Key::Named(key),
        modifiers: bare,
        command: false,
    })
    .collect::<Vec<_>>();
    claims.extend(
        [
            ("z", false),
            ("z", true),
            ("y", false),
            ("b", false),
            ("i", false),
            ("c", true),
            ("9", true),
            ("v", false),
            ("c", false),
            ("x", false),
        ]
        .into_iter()
        .map(|(key, shift)| wire::EditorKeyClaim {
            key: Key::Character(key.into()),
            modifiers: Modifiers { shift, ..bare },
            command: true,
        }),
    );
    let deciding = draft.clone();
    let decide_choices = choices.clone();
    let observing = draft.clone();
    let observed_choices = choices.clone();
    let interacting = draft.clone();
    let on_committed = wrap.clone();
    let on_transaction = wrap.clone();
    let binding = EditorBinding::new(
        claims,
        move |request| {
            if !editable {
                return wire::EditorDecision::Noop;
            }
            let tag = key_tag(&deciding, &decide_choices, request.state, request.key);
            if tag == "send" && request.repeat {
                return wire::EditorDecision::Noop;
            }
            deciding.decide(&tag, &decide_choices, request.state)
        },
        move |event| match event {
            EditorTransactionEvent::Commit {
                before,
                after,
                origin,
                ..
            } => {
                let tag = match origin {
                    Some(wire::EditorRequestInput::Key { key, .. }) => {
                        key_tag(&observing, &observed_choices, before, key)
                    }
                    Some(wire::EditorRequestInput::Interaction {
                        action: wire::editor_presentation::EditorInteraction::Action { tag },
                    }) => tag.clone(),
                    _ => String::new(),
                };
                Some(Change {
                    before: before.text.into(),
                    after: after.text.into(),
                    cursor: before.cursor,
                    tag,
                })
            }
            EditorTransactionEvent::Interaction { .. }
            | EditorTransactionEvent::Fault { .. }
            | EditorTransactionEvent::Cancelled { .. } => None,
        },
    )
    .on_interaction(move |request| {
        if !editable {
            return wire::EditorDecision::Noop;
        }
        match request.action {
            wire::editor_presentation::EditorInteraction::Action { tag } => {
                interacting.decide(tag, &choices, request.state)
            }
            _ => wire::EditorDecision::Noop,
        }
    })
    .register(
        move |change| on_committed(Event::Committed(change)),
        move |transaction| on_transaction(Event::Transaction(transaction)),
    );
    let mut presentation = wire::editor_presentation::EditorPresentation {
        formats: vec![wire::editor_presentation::EditorFormat {
            color: Some(wire::Rgba([0.25, 0.55, 0.95, 1.])),
            ..Default::default()
        }],
        ..Default::default()
    };
    for mention in &draft.mentions {
        let start = editing::position(draft.editor.state_view().text, mention.range.start);
        let end = editing::position(draft.editor.state_view().text, mention.range.end);
        if start.line == end.line {
            presentation
                .spans
                .push(wire::editor_presentation::EditorSpan {
                    line: start.line,
                    start: start.column,
                    end: end.column,
                    format: 0,
                });
        }
    }
    wire::Node::Editor {
        key: key.into(),
        document,
        on_document,
        editable,
        placeholder: placeholder.into(),
        width: None,
        height: None,
        min_height: Some(64.),
        max_height: Some(240.),
        options: Box::new(wire::EditorOptions {
            binding: Some(Box::new(binding)),
            presentation: Some(Box::new(presentation)),
            size: Some(14.),
            padding: Some(8.),
            wrapping: Some(wire::Wrapping::Word),
            ..Default::default()
        }),
    }
}

pub fn view<M: Clone + 'static>(
    draft: &Draft,
    key: &str,
    hint: &str,
    editable: bool,
    choices: &[MentionChoice],
    wrap: impl Fn(Event<M>) -> M + Clone + 'static,
) -> wire::Node {
    let editor_key = format!("{key}/editor");
    let mut controls = Vec::new();
    for (tag, label) in [
        ("bold", "Bold"),
        ("italic", "Italic"),
        ("code", "Code"),
        ("quote", "Quote"),
        ("attach", "Attach"),
        ("send", "Send"),
    ] {
        controls.push(kit::button(
            format!("{key}/{tag}"),
            label,
            editable.then(|| slots::message(wrap(Event::Action(tag.into())))),
            wire::ButtonPreset::Text,
        ));
    }
    let mut children = vec![
        editor(draft, &editor_key, hint, editable, choices, wrap.clone()),
        kit::row(format!("{key}/toolbar"), controls),
    ];
    if let Some((_, partial)) = draft.query(draft.editor.state_view()) {
        let choices = matching_choices(choices, &partial);
        let selected = draft.menu_index.min(choices.len().saturating_sub(1));
        for (index, choice) in choices.into_iter().enumerate() {
            children.push(kit::button(
                format!("{key}/mention/{}", choice.token),
                &choice.label,
                editable.then(|| {
                    slots::message(wrap(Event::Action(format!("mention:{}", choice.token))))
                }),
                if index == selected {
                    wire::ButtonPreset::Primary
                } else {
                    wire::ButtonPreset::Text
                },
            ));
        }
    }
    for attachment in &draft.attachments {
        let status = match &attachment.state {
            super::AttachmentState::Uploading => "Uploading".to_owned(),
            super::AttachmentState::Ready { .. } => "Ready".to_owned(),
            super::AttachmentState::Failed { reason } => reason.clone(),
            super::AttachmentState::Unavailable => "Select the file again to upload".into(),
        };
        children.push(kit::text(
            format!("{key}/attachment/{}", attachment.token),
            format!("{} — {status}", attachment.name),
        ));
        children.push(kit::button(
            format!("{key}/remove/{}", attachment.token),
            "Remove",
            Some(slots::message(wrap(Event::Action(format!(
                "remove:{}",
                attachment.token
            ))))),
            wire::ButtonPreset::Text,
        ));
        if matches!(attachment.state, super::AttachmentState::Failed { .. }) {
            children.push(kit::button(
                format!("{key}/retry/{}", attachment.token),
                "Retry",
                Some(slots::message(wrap(Event::Action(format!(
                    "retry:{}",
                    attachment.token
                ))))),
                wire::ButtonPreset::Text,
            ));
        }
    }
    if draft.failed_send.is_some() {
        children.push(kit::button(
            format!("{key}/restore"),
            "Restore unsent message",
            Some(slots::message(wrap(Event::Action("restore".into())))),
            wire::ButtonPreset::Text,
        ));
    }
    if !draft.note.is_empty() {
        children.push(kit::text(format!("{key}/note"), &draft.note));
    }
    kit::column(key, children)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_navigation_commits_before_enter_chooses_a_stable_identity() {
        let choices = vec![
            MentionChoice {
                token: "<@1>".into(),
                label: "Ada".into(),
            },
            MentionChoice {
                token: "<@2>".into(),
                label: "Alan".into(),
            },
        ];
        let mut draft = Draft::from_body("@A", &choices);
        draft.editor.move_to(wire::EditorCursor {
            position: wire::EditorPosition { line: 0, column: 2 },
            selection: None,
        });
        let cursor = draft.editor.cursor();
        draft.committed("@A", "@A", cursor, "menu-next", &choices);
        let key = wire::keyboard::KeyState {
            key: Key::Named(Named::Enter),
            modifiers: Modifiers::default(),
            modified_key: Key::Named(Named::Enter),
            physical_key: wire::keyboard::Physical::Unidentified(
                wire::keyboard::NativeCode::Unidentified,
            ),
            location: wire::keyboard::Location::Standard,
        };
        assert_eq!(
            key_tag(&draft, &choices, draft.editor.state_view(), &key),
            "mention:<@2>"
        );
        draft.committed("@A", "@A", cursor, "menu-dismiss", &choices);
        assert_eq!(
            key_tag(&draft, &choices, draft.editor.state_view(), &key),
            "send"
        );
        draft.observed("@A", "@Al");
        assert!(!draft.menu_dismissed);
    }
}
