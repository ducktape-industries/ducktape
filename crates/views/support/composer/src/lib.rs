//! Guest-owned rich composer state shared by conversation views.

mod binding;
mod editing;
pub mod host;
pub use binding::{Event, Outcome, view};

use ducktape_view_guest::{Editor, wire};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionChoice {
    pub token: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    pub range: Range<usize>,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentState {
    Uploading,
    Ready { uri: String },
    Failed { reason: String },
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub token: String,
    pub name: String,
    pub bytes: u64,
    pub state: AttachmentState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Send {
    pub body: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct History {
    pub text: String,
    pub mentions: Vec<Mention>,
    pub cursor: wire::EditorCursor,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Draft {
    #[serde(with = "document_snapshot")]
    pub editor: Editor,
    pub mentions: Vec<Mention>,
    pub attachments: Vec<Attachment>,
    pub failed_send: Option<Send>,
    pub submitted: Option<Send>,
    pub in_flight: Vec<Send>,
    pub note: String,
    pub paste: Option<String>,
    pub clipboard: Option<String>,
    pub menu_index: usize,
    pub menu_dismissed: bool,
    pub undo: Vec<History>,
    pub redo: Vec<History>,
}

impl Draft {
    /// A draft holding one of everything, for the snapshot-schema trace the
    /// views that carry a draft each run. Its editor document restores (the
    /// byte a tracer makes up does not), and no collection or option is empty,
    /// because a shape the sample does not reach is a shape the tag does not
    /// describe. The literal is exhaustive on purpose: a field added to
    /// `Draft` is a field the compiler makes you reach here too.
    pub fn schema_sample() -> Self {
        let attachments = vec![
            Attachment {
                token: "t".into(),
                name: "a".into(),
                bytes: 1,
                state: AttachmentState::Uploading,
            },
            Attachment {
                token: "t".into(),
                name: "a".into(),
                bytes: 1,
                state: AttachmentState::Ready { uri: "u".into() },
            },
            Attachment {
                token: "t".into(),
                name: "a".into(),
                bytes: 1,
                state: AttachmentState::Failed { reason: "r".into() },
            },
            Attachment {
                token: "t".into(),
                name: "a".into(),
                bytes: 1,
                state: AttachmentState::Unavailable,
            },
        ];
        let mentions = vec![Mention {
            range: 0..1,
            token: "<@1>".into(),
        }];
        let send = Send {
            body: "b".into(),
            attachments: attachments.clone(),
        };
        let history = vec![History {
            text: "b".into(),
            mentions: mentions.clone(),
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 0, column: 1 },
                selection: Some(wire::EditorPosition { line: 0, column: 0 }),
            },
        }];
        Self {
            editor: Editor::new("b"),
            mentions,
            attachments,
            failed_send: Some(send.clone()),
            submitted: Some(send.clone()),
            in_flight: vec![send],
            note: "n".into(),
            paste: Some("p".into()),
            clipboard: Some("c".into()),
            menu_index: 0,
            menu_dismissed: false,
            undo: history.clone(),
            redo: history,
        }
    }

    pub fn from_body(body: &str, roster: &[MentionChoice]) -> Self {
        let mut draft = Self::default();
        draft.seed(body, roster);
        draft
    }

    pub fn seed(&mut self, body: &str, roster: &[MentionChoice]) {
        let body = body.replace("\r\n", "\n").replace('\r', "\n");
        let mut text = String::new();
        let mut mentions = Vec::new();
        let mut remaining = body.as_str();
        while let Some(start) = remaining.find("<@") {
            text.push_str(&remaining[..start]);
            remaining = &remaining[start..];
            let Some(end) = remaining.find('>') else {
                break;
            };
            let token = &remaining[..=end];
            match roster.iter().find(|choice| choice.token == token) {
                Some(choice) => {
                    let start = text.len();
                    text.push('@');
                    text.push_str(&choice.label);
                    mentions.push(Mention {
                        range: start..text.len(),
                        token: token.into(),
                    });
                }
                None => text.push_str(token),
            }
            remaining = &remaining[end + 1..];
        }
        text.push_str(remaining);
        self.editor
            .replace(Editor::new(text), self.editor.reset_revision());
        self.mentions = mentions;
        self.undo.clear();
        self.redo.clear();
    }

    pub fn body(&self) -> String {
        self.body_of(self.editor.state_view().text)
    }

    pub(crate) fn body_of(&self, text: &str) -> String {
        let mut body = String::new();
        let mut start = 0;
        for mention in &self.mentions {
            body.push_str(&text[start..mention.range.start]);
            body.push_str(&mention.token);
            start = mention.range.end;
        }
        body.push_str(&text[start..]);
        body
    }

    pub fn observed(&mut self, before: &str, after: &str) {
        if before != after {
            self.menu_index = 0;
            self.menu_dismissed = false;
        }
        let Ok(patches) = wire::editor_document::editor_changed_span(before, after) else {
            return;
        };
        for patch in patches.into_iter().rev() {
            let replaced = patch.start_byte as usize..patch.end_byte as usize;
            self.mentions.retain_mut(|mention| {
                if mention.range.end <= replaced.start {
                    return true;
                }
                if mention.range.start >= replaced.end {
                    mention.range.start =
                        replaced.start + patch.replacement.len() + mention.range.start
                            - replaced.end;
                    mention.range.end =
                        replaced.start + patch.replacement.len() + mention.range.end - replaced.end;
                    return true;
                }
                false
            });
        }
    }

    pub(crate) fn can_send(&self, text: &str) -> bool {
        let uploading = self
            .attachments
            .iter()
            .any(|file| file.state == AttachmentState::Uploading);
        let ready = self
            .attachments
            .iter()
            .any(|file| matches!(file.state, AttachmentState::Ready { .. }));
        !uploading && (!text.trim().is_empty() || ready)
    }

    pub fn failed(&mut self, send: Send) {
        match &mut self.failed_send {
            Some(previous) => {
                if !previous.body.is_empty() && !send.body.is_empty() {
                    previous.body.push('\n');
                }
                previous.body.push_str(&send.body);
                previous.attachments.extend(send.attachments);
            }
            None => self.failed_send = Some(send),
        }
    }

    pub fn complete_send(&mut self, send: &Send) {
        let Some(index) = self.in_flight.iter().position(|pending| pending == send) else {
            return;
        };
        self.in_flight.remove(index);
    }

    pub fn retire_device_requests(&mut self) {
        for send in std::mem::take(&mut self.in_flight) {
            self.failed(send);
            self.note = "A send was interrupted; check the conversation before restoring it".into();
        }
        self.paste = None;
        self.clipboard = None;
        for file in &mut self.attachments {
            if matches!(
                file.state,
                AttachmentState::Uploading | AttachmentState::Failed { .. }
            ) {
                file.state = AttachmentState::Unavailable;
            }
        }
    }
}

mod document_snapshot {
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        editor: &ducktape_view_guest::Editor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        editor.snapshot().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ducktape_view_guest::Editor, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        ducktape_view_guest::Editor::restore(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid composer document"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_wraps_the_latest_selection_without_losing_mention_identity() {
        let choices = vec![MentionChoice {
            token: "<@7>".into(),
            label: "Ada".into(),
        }];
        let mut draft = Draft::from_body("Hi <@7>", &choices);
        draft.editor.move_to(wire::EditorCursor {
            position: wire::EditorPosition { line: 0, column: 7 },
            selection: Some(wire::EditorPosition { line: 0, column: 3 }),
        });
        let before = draft.editor.text();
        let cursor = draft.editor.cursor();
        let wire::EditorDecision::Apply {
            patches,
            cursor: next,
            ..
        } = draft.decide("bold", &choices, draft.editor.state_view())
        else {
            panic!("format decision");
        };
        let after = wire::patched_editor_text(&before, &patches, next).unwrap();
        draft.committed(&before, &after, cursor, "bold", &choices);
        draft
            .editor
            .replace(Editor::new(after), draft.editor.reset_revision());
        assert_eq!(draft.body(), "Hi **<@7>**");
    }

    #[test]
    fn replacement_retains_the_bodies_of_interrupted_send_tasks() {
        let mut draft = Draft::from_body("new typing", &[]);
        draft.in_flight.push(Send {
            body: "in flight".into(),
            attachments: Vec::new(),
        });
        let mut restored: Draft =
            serde_json::from_slice(&serde_json::to_vec(&draft).unwrap()).unwrap();
        restored.retire_device_requests();
        assert_eq!(restored.editor.text(), "new typing");
        assert_eq!(restored.failed_send.as_ref().unwrap().body, "in flight");
        assert!(restored.in_flight.is_empty());
    }

    #[test]
    fn replacement_revokes_pending_device_work_but_keeps_uploaded_links() {
        let mut draft = Draft::from_body("still typing", &[]);
        draft.attachments = vec![
            Attachment {
                token: "a".into(),
                name: "pending".into(),
                bytes: 1,
                state: AttachmentState::Uploading,
            },
            Attachment {
                token: "b".into(),
                name: "ready".into(),
                bytes: 1,
                state: AttachmentState::Ready {
                    uri: "duck://files/x".into(),
                },
            },
        ];
        let mut restored: Draft =
            serde_json::from_slice(&serde_json::to_vec(&draft).unwrap()).unwrap();
        restored.retire_device_requests();
        assert_eq!(restored.editor.text(), "still typing");
        assert_eq!(restored.attachments[0].state, AttachmentState::Unavailable);
        assert_eq!(restored.attachments[1].state, draft.attachments[1].state);
    }

    #[test]
    fn two_failed_sends_preserve_both_bodies_and_restore_cannot_erase_new_typing() {
        let mut draft = Draft::from_body("new typing", &[]);
        for body in ["first", "second"] {
            draft.failed(Send {
                body: body.into(),
                attachments: Vec::new(),
            });
        }
        assert_eq!(draft.failed_send.as_ref().unwrap().body, "first\nsecond");
        assert_eq!(
            draft.decide("restore", &[], draft.editor.state_view()),
            wire::EditorDecision::Noop
        );
        assert_eq!(draft.editor.text(), "new typing");
    }

    #[test]
    fn undo_restores_the_identity_removed_by_an_atomic_delete() {
        let choices = vec![MentionChoice {
            token: "<@7>".into(),
            label: "Ada".into(),
        }];
        let mut draft = Draft::from_body("@literal <@7>", &choices);
        let cursor = wire::EditorCursor {
            position: wire::EditorPosition {
                line: 0,
                column: 13,
            },
            selection: None,
        };
        draft.committed("@literal @Ada", "@literal ", cursor, "backspace", &choices);
        draft
            .editor
            .replace(Editor::new("@literal "), draft.editor.reset_revision());
        let wire::EditorDecision::Apply {
            patches,
            cursor: next,
            ..
        } = draft.decide("undo", &choices, draft.editor.state_view())
        else {
            panic!("undo decision")
        };
        let after = wire::patched_editor_text("@literal ", &patches, next).unwrap();
        draft.committed("@literal ", &after, draft.editor.cursor(), "undo", &choices);
        draft
            .editor
            .replace(Editor::new(after), draft.editor.reset_revision());
        assert_eq!(draft.body(), "@literal <@7>");
    }

    #[test]
    fn delayed_paste_uses_current_selection_and_preserves_stable_mentions() {
        let choices = vec![MentionChoice {
            token: "<@7>".into(),
            label: "Ada".into(),
        }];
        let mut draft = Draft::from_body("new typing ", &choices);
        draft.editor.move_to(wire::EditorCursor {
            position: wire::EditorPosition {
                line: 0,
                column: 11,
            },
            selection: None,
        });
        draft.paste = Some("hello <@7>".into());
        let before = draft.editor.text();
        let old_cursor = draft.editor.cursor();
        let wire::EditorDecision::Apply {
            patches, cursor, ..
        } = draft.decide("paste-ready", &choices, draft.editor.state_view())
        else {
            panic!("paste decision")
        };
        let after = wire::patched_editor_text(&before, &patches, cursor).unwrap();
        draft.committed(&before, &after, old_cursor, "paste-ready", &choices);
        draft
            .editor
            .replace(Editor::new(after), draft.editor.reset_revision());
        assert_eq!(draft.body(), "new typing hello <@7>");
        assert!(draft.paste.is_none());
    }

    #[test]
    fn restored_drafts_keep_stable_mentions_when_labels_change() {
        let roster = vec![MentionChoice {
            token: "<@7>".into(),
            label: "Ada".into(),
        }];
        let draft = Draft::from_body("Hello <@7>", &roster);
        assert_eq!(draft.editor.text(), "Hello @Ada");
        assert_eq!(draft.body(), "Hello <@7>");
        let restored: Draft = serde_json::from_slice(&serde_json::to_vec(&draft).unwrap()).unwrap();
        assert_eq!(restored.body(), "Hello <@7>");
    }

    #[test]
    fn queued_upload_blocks_send_and_failure_preserves_newer_typing() {
        let mut draft = Draft::from_body("first", &[]);
        draft.attachments.push(Attachment {
            token: "f1".into(),
            name: "a.txt".into(),
            bytes: 10,
            state: AttachmentState::Uploading,
        });
        assert!(!draft.can_send(draft.editor.state_view().text));
        draft.attachments[0].state = AttachmentState::Ready {
            uri: "duck://files/a.txt".into(),
        };
        let before = draft.editor.text();
        draft.committed(&before, "", draft.editor.cursor(), "send", &[]);
        let sent = draft.submitted.take().unwrap();
        draft
            .editor
            .replace(Editor::new(""), draft.editor.reset_revision());
        assert_eq!(sent.body, "first");
        assert!(draft.editor.text().is_empty());
        draft.seed("second", &[]);
        draft.failed(sent);
        assert_eq!(draft.editor.text(), "second");
        assert_eq!(draft.failed_send.as_ref().unwrap().body, "first");
    }

    #[test]
    fn deleting_a_mention_removes_identity_and_moves_following_ranges() {
        let roster = vec![MentionChoice {
            token: "<@7>".into(),
            label: "Ada".into(),
        }];
        let mut draft = Draft::from_body("<@7> and <@7>", &roster);
        draft.observed("@Ada and @Ada", " and @Ada");
        draft.editor.replace(
            ducktape_view_guest::Editor::new(" and @Ada"),
            draft.editor.reset_revision(),
        );
        assert_eq!(draft.body(), " and <@7>");
    }
}
