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
    // a mention wears the product's accent, the same one a chosen row and a
    // live dot wear — not a colour of this module's own
    let mut presentation = wire::editor_presentation::EditorPresentation {
        formats: vec![wire::editor_presentation::EditorFormat {
            color: Some(kit::rgba(kit::palette().accent)),
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
        // one row of body text, and room to grow to about eight before the
        // field scrolls instead of eating the timeline
        min_height: Some(40.),
        max_height: Some(200.),
        options: Box::new(wire::EditorOptions {
            binding: Some(Box::new(binding)),
            presentation: Some(Box::new(presentation)),
            // the body size every view writes at, not a size of its own
            size: Some(kit::type_scale::BODY as f32),
            // This IS the draft's text inset — the host pads the field's box
            // by it and the text control adds nothing of its own, so at 0 the
            // first letter sits on the plate's border (seen on a live app,
            // 2026-09-16). Vertically it is also the air above the first row,
            // which is why `min_height` is exactly one row plus twice this.
            padding: Some(TEXT_INSET),
            wrapping: Some(wire::Wrapping::Word),
            ..Default::default()
        }),
    }
}

/// Where the draft's text starts, from the field's left edge: the native
/// field pads its own text this far, and every row under it lines up there.
const TEXT_INSET: f32 = 10.;
/// A row of controls stops short of that line, because a square control
/// centres its sign and so carries the rest of the distance inside its own
/// box. Aligning the BOXES would push every sign a glyph's width to the
/// right of the draft's first letter.
const CONTROL_INSET: f32 = 4.;
/// A mark button is a square holding one sign, and tall enough that the
/// host's button does not clip the sign to its line box.
const MARK: f32 = 24.;

/// One mark a draft can carry: a quiet square holding a single typographic
/// sign. The sign is what a reader sees; `name` is what a screen reader
/// hears, since a sign is not a word.
fn mark(key: String, sign: &str, name: &str, on_press: Option<u32>) -> wire::Node {
    let mut button = kit::button_child(
        key.clone(),
        kit::nowrap(kit::text_options(
            kit::text_size(
                kit::text(format!("{key}/sign"), sign),
                kit::type_scale::BODY as f32,
            ),
            wire::TextOptions {
                line_height: Some(wire::LineHeight::Absolute(MARK)),
                ..Default::default()
            },
        )),
        on_press,
        wire::ButtonPreset::Subtle,
    );
    let wire::Node::Button {
        label,
        width,
        height,
        padding,
        ..
    } = &mut button
    else {
        unreachable!()
    };
    *label = Some(name.into());
    *width = Some(wire::Length::Fixed(MARK));
    *height = Some(wire::Length::Fixed(MARK));
    *padding = Some(wire::Edges::all(0.));
    button
}

/// A file the draft is carrying: its name over what became of it, and the
/// way to take it back out — one chip, not three loose lines.
fn chip(key: &str, name: &str, note: &str, tone: kit::Tone, remove: Option<u32>) -> wire::Node {
    let p = kit::palette();
    let mut body = kit::spaced(
        kit::column(
            format!("{key}/body"),
            [
                kit::nowrap(kit::weighted(
                    kit::text_size(
                        kit::text(format!("{key}/name"), name),
                        kit::type_scale::SECONDARY as f32,
                    ),
                    wire::Weight::Medium,
                )),
                kit::nowrap(kit::colored(
                    kit::text_size(
                        kit::text(format!("{key}/note"), note),
                        kit::type_scale::CAPTION as f32,
                    ),
                    tone.color(p),
                )),
            ],
        ),
        1.,
    );
    body = kit::sized(body, Some(wire::Length::Shrink), None);
    let mut chip = kit::container(
        format!("{key}/chip"),
        kit::spaced(
            kit::centered_row(
                format!("{key}/row"),
                [body, mark(format!("{key}/remove"), "×", "Remove", remove)],
            ),
            4.,
        ),
    );
    let wire::Node::Container {
        border,
        background,
        padding,
        width,
        ..
    } = &mut chip
    else {
        unreachable!()
    };
    *border = Some(wire::Border {
        color: Some(kit::rgba(p.border)),
        width: Some(1.),
        radius: Some([kit::radius::CONTROL as f32; 4]),
    });
    *background = Some(wire::Background::Color(kit::rgba(p.surface)));
    *padding = Some(wire::Edges {
        top: 4.,
        right: 4.,
        bottom: 4.,
        left: 8.,
    });
    *width = Some(wire::Length::Shrink);
    chip
}

/// The draft, everything it carries, and the row that sends it — one
/// plate reading down to a single action on the right.
///
/// The plate is drawn HERE. The host mounts the field as a bare text
/// surface with no border and no fill of its own (verified on a live app,
/// 2026-09-16), so a composer that draws nothing is a placeholder and a
/// row of controls floating loose on the timeline's own background, which
/// is what this replaced.
pub fn view<M: Clone + 'static>(
    draft: &Draft,
    key: &str,
    hint: &str,
    editable: bool,
    choices: &[MentionChoice],
    wrap: impl Fn(Event<M>) -> M + Clone + 'static,
) -> wire::Node {
    let editor_key = format!("{key}/editor");
    let press = |tag: String| -> Option<u32> {
        let wrap = wrap.clone();
        editable.then(|| slots::message(wrap(Event::Action(tag))))
    };
    let text = draft.editor.state_view().text;
    let carries_file = draft
        .attachments
        .iter()
        .any(|held| matches!(held.state, super::AttachmentState::Ready { .. }));
    let waits_on_upload = draft
        .attachments
        .iter()
        .any(|held| held.state == super::AttachmentState::Uploading);
    // a send needs something to say, and waits for its files to land
    let sendable = editable && (!text.trim().is_empty() || carries_file) && !waits_on_upload;

    let mut rows = Vec::new();
    // The choices sit ABOVE the draft: picking one must not slide the row
    // of controls out from under the reader's pointer.
    if let Some((_, partial)) = draft.query(draft.editor.state_view()) {
        let matches = matching_choices(choices, &partial);
        let selected = draft.menu_index.min(matches.len().saturating_sub(1));
        let picks: Vec<wire::Node> = matches
            .into_iter()
            .enumerate()
            .map(|(index, choice)| {
                let row = format!("{key}/mention/{}", choice.token);
                // a handle reads from the left; the host centres a button's
                // child, so a spacer after the name pushes it back over
                kit::list_row(
                    row.clone(),
                    kit::row(
                        format!("{row}/row"),
                        [
                            kit::nowrap(kit::text(
                                format!("{row}/name"),
                                format!("@{}", choice.label),
                            )),
                            kit::spacer(),
                        ],
                    ),
                    index == selected,
                    press(format!("mention:{}", choice.token)),
                )
            })
            .collect();
        if !picks.is_empty() {
            rows.push(kit::spaced(
                kit::column(format!("{key}/mentions"), picks),
                1.,
            ));
        }
    }
    rows.push(editor(
        draft,
        &editor_key,
        hint,
        editable,
        choices,
        wrap.clone(),
    ));
    if !draft.attachments.is_empty() {
        let chips: Vec<wire::Node> = draft
            .attachments
            .iter()
            .map(|held| {
                let at = format!("{key}/attachment/{}", held.token);
                let (note, tone) = match &held.state {
                    super::AttachmentState::Uploading => ("Uploading…".to_owned(), kit::Tone::Neutral),
                    super::AttachmentState::Ready { uri } => (uri.clone(), kit::Tone::Neutral),
                    super::AttachmentState::Failed { reason } => (reason.clone(), kit::Tone::Danger),
                    super::AttachmentState::Unavailable => {
                        ("Select the file again".to_owned(), kit::Tone::Warning)
                    }
                };
                let mut carried = vec![chip(
                    &at,
                    &held.name,
                    &note,
                    tone,
                    press(format!("remove:{}", held.token)),
                )];
                // a failure is the one state with a way out of it
                if matches!(held.state, super::AttachmentState::Failed { .. }) {
                    carried.push(kit::button(
                        format!("{at}/retry"),
                        "Retry",
                        press(format!("retry:{}", held.token)),
                        wire::ButtonPreset::Subtle,
                    ));
                }
                kit::spaced(kit::centered_row(format!("{at}/held"), carried), 4.)
            })
            .collect();
        rows.push(inset(
            kit::spaced(kit::wrapped_row(format!("{key}/attachments"), chips), 6.),
            TEXT_INSET,
        ));
    }
    if !draft.note.is_empty() {
        rows.push(inset(
            kit::row(
                format!("{key}/note-row"),
                [kit::wrapping(kit::text_size(
                    kit::tone_text(format!("{key}/note"), &draft.note, kit::Tone::Danger),
                    kit::type_scale::SECONDARY as f32,
                ))],
            ),
            TEXT_INSET,
        ));
    }
    if draft.failed_send.is_some() {
        rows.push(inset(
            kit::notice(
                format!("{key}/failed"),
                kit::spaced(
                    kit::centered_row(
                        format!("{key}/failed/row"),
                        [
                            kit::text(format!("{key}/failed/note"), "An earlier message wasn’t sent"),
                            kit::spacer(),
                            kit::button(
                                format!("{key}/restore"),
                                "Restore",
                                press("restore".into()),
                                wire::ButtonPreset::Subtle,
                            ),
                        ],
                    ),
                    8.,
                ),
                kit::Tone::Danger,
            ),
            0.,
        ));
    }
    // What the draft can carry, then the one action that sends it. Each
    // mark is a sign rather than a word: five words in a row read as a
    // sentence, five signs read as a toolbar.
    let mut controls = vec![
        mark(format!("{key}/attach"), "+", "Attach a file", press("attach".into())),
        mark(format!("{key}/bold"), "B", "Bold", press("bold".into())),
        mark(format!("{key}/italic"), "I", "Italic", press("italic".into())),
        // Latin punctuation only: the product face carries it. A dingbat
        // quote mark (❞) or an angle-quote pair (‹›) falls out of Inter and
        // lands in whatever the system has, which is a tofu box on a host
        // with no fallback and an emoji on one that has too much.
        mark(format!("{key}/code"), "<>", "Code", press("code".into())),
        mark(format!("{key}/quote"), "”", "Quote", press("quote".into())),
        kit::spacer(),
    ];
    controls.push(kit::button(
        format!("{key}/send"),
        "Send",
        sendable.then(|| slots::message(wrap(Event::Action("send".into())))),
        wire::ButtonPreset::Primary,
    ));
    rows.push(inset(
        kit::spaced(kit::centered_row(format!("{key}/toolbar"), controls), 2.),
        CONTROL_INSET,
    ));
    plate(key, kit::spaced(kit::column(format!("{key}/rows"), rows), 6.))
}

/// The box the whole draft lives in: the window's own colour inside a
/// control's hairline. It pads nothing — the field pads its own text and
/// every other row reaches that line itself, so one inset governs.
fn plate(key: &str, child: wire::Node) -> wire::Node {
    let p = kit::palette();
    let mut node = kit::container(key, child);
    let wire::Node::Container {
        border,
        background,
        padding,
        ..
    } = &mut node
    else {
        unreachable!()
    };
    *border = Some(wire::Border {
        color: Some(kit::rgba(p.border_strong)),
        width: Some(1.),
        radius: Some([kit::radius::CARD as f32; 4]),
    });
    *background = Some(wire::Background::Color(kit::rgba(p.background)));
    *padding = Some(wire::Edges {
        top: 0.,
        right: 0.,
        bottom: CONTROL_INSET,
        left: 0.,
    });
    node
}

/// A row under the field, moved in to the line the draft's own text sits
/// on. The column's spacing owns the air between rows, so nothing here
/// pads its own top or bottom — two sources of vertical rhythm is how a
/// stack ends up with three different gaps in it.
fn inset(node: wire::Node, sides: f32) -> wire::Node {
    kit::padded(
        node,
        wire::Edges {
            top: 0.,
            right: sides,
            bottom: 0.,
            left: sides,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every node under `node`, itself first.
    fn walk(node: &wire::Node, seen: &mut impl FnMut(&wire::Node)) {
        seen(node);
        match node {
            wire::Node::Linear { children, .. } => {
                for child in children {
                    walk(child, seen);
                }
            }
            wire::Node::Container { content, .. } => walk(content, seen),
            wire::Node::Button {
                content: wire::ButtonContent::Child(child),
                ..
            } => walk(child, seen),
            _ => {}
        }
    }

    fn drawn(draft: &Draft) -> wire::Node {
        view(draft, "c", "Message #general", true, &[], |_: Event<()>| ())
    }

    /// The composer's shape is a claim a reader can see at a glance: ONE
    /// action is the action, and it is dead until there is something to
    /// send. Six identical buttons in a row is the shape this replaced.
    #[test]
    fn the_send_is_the_only_primary_and_is_dead_on_an_empty_draft() {
        let primaries = |draft: &Draft| {
            let mut found: Vec<(String, Option<u32>)> = Vec::new();
            walk(&drawn(draft), &mut |node| {
                if let wire::Node::Button {
                    key,
                    style,
                    on_press,
                    ..
                } = node
                    && style.preset == wire::ButtonPreset::Primary
                {
                    found.push((key.clone(), *on_press));
                }
            });
            found
        };
        let empty = primaries(&Draft::default());
        assert_eq!(empty.len(), 1, "one primary action, not six: {empty:?}");
        assert_eq!(empty[0].0, "c/send");
        assert!(empty[0].1.is_none(), "an empty draft cannot be sent");
        let typed = primaries(&Draft::from_body("hello", &[]));
        assert!(typed[0].1.is_some(), "a draft with words can be sent");
    }

    /// The marks are squares of one size. A mark that takes its size from
    /// its glyph gives a toolbar of five different boxes.
    #[test]
    fn every_mark_is_the_same_square_and_the_field_writes_at_body_size() {
        let mut squares = 0;
        let mut body_size = None;
        walk(&drawn(&Draft::default()), &mut |node| match node {
            wire::Node::Button { width, height, .. }
                if *width == Some(wire::Length::Fixed(MARK))
                    && *height == Some(wire::Length::Fixed(MARK)) =>
            {
                squares += 1;
            }
            wire::Node::Editor { options, .. } => body_size = options.size,
            _ => {}
        });
        // attach, bold, italic, code, quote
        assert_eq!(squares, 5, "five marks, all one square");
        assert_eq!(body_size, Some(kit::type_scale::BODY as f32));
    }

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
