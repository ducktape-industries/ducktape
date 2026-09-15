//! Pure composer decisions. Native input enters through the editor transaction lane.
use super::{Draft, Mention, MentionChoice};
use ducktape_view_guest::{EditorStateView, wire};
use std::ops::Range;

pub(crate) fn offset(text: &str, position: wire::EditorPosition) -> usize {
    text.split_inclusive('\n')
        .take(position.line as usize)
        .map(str::len)
        .sum::<usize>()
        + position.column as usize
}
pub(crate) fn position(text: &str, at: usize) -> wire::EditorPosition {
    let before = &text[..at];
    wire::EditorPosition {
        line: before.bytes().filter(|byte| *byte == b'\n').count() as u32,
        column: before.rsplit('\n').next().unwrap_or_default().len() as u32,
    }
}
pub(crate) fn selection(text: &str, cursor: wire::EditorCursor) -> Range<usize> {
    let at = offset(text, cursor.position);
    let anchor = cursor.selection.map_or(at, |anchor| offset(text, anchor));
    at.min(anchor)..at.max(anchor)
}
fn markers(tag: &str) -> Option<(&'static str, &'static str)> {
    match tag {
        "bold" => Some(("**", "**")),
        "italic" => Some(("_", "_")),
        "code" => Some(("```\n", "\n```")),
        "quote" => Some(("> ", "")),
        _ => None,
    }
}
fn apply(
    state: EditorStateView<'_>,
    range: Range<usize>,
    replacement: String,
    caret: usize,
) -> wire::EditorDecision {
    let mut text = state.text.to_owned();
    text.replace_range(range.clone(), &replacement);
    wire::EditorDecision::Apply {
        patches: vec![wire::EditorPatch {
            start_byte: range.start as u32,
            end_byte: range.end as u32,
            replacement,
        }],
        cursor: wire::EditorCursor {
            position: position(&text, caret),
            selection: None,
        },
        history: wire::EditorHistoryEffect::Native,
    }
}

impl Draft {
    fn expanded(&self, mut range: Range<usize>, tag: &str) -> Range<usize> {
        for mention in &self.mentions {
            let overlaps = range.start < mention.range.end && range.end > mention.range.start;
            let inside = range.is_empty()
                && range.start > mention.range.start
                && range.start < mention.range.end;
            let removes_boundary = range.is_empty()
                && ((tag == "backspace" && range.start == mention.range.end)
                    || (tag == "delete" && range.start == mention.range.start));
            if overlaps || inside || removes_boundary {
                range.start = range.start.min(mention.range.start);
                range.end = range.end.max(mention.range.end);
            }
        }
        range
    }

    pub(crate) fn query(&self, state: EditorStateView<'_>) -> Option<(Range<usize>, String)> {
        if self.menu_dismissed {
            return None;
        }
        let range = selection(state.text, state.cursor);
        if !range.is_empty() {
            return None;
        }
        let before = state.text.get(..range.start)?;
        let after = state.text.get(range.end..)?;
        let handle_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
        if after.chars().next().is_some_and(handle_char) {
            return None;
        }
        let line = before.rsplit('\n').next()?;
        let at = line.rfind('@')?;
        let partial = &line[at + 1..];
        let valid = partial.chars().all(|c| handle_char(c) || c == ' ')
            && line[..at]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace);
        if !valid {
            return None;
        }
        let range = before.len() - line.len() + at..range.end;
        let overlaps = self
            .mentions
            .iter()
            .any(|mention| range.start < mention.range.end && range.end > mention.range.start);
        if overlaps {
            return None;
        }
        Some((range, partial.into()))
    }

    pub fn decide(
        &self,
        tag: &str,
        choices: &[MentionChoice],
        state: EditorStateView<'_>,
    ) -> wire::EditorDecision {
        if matches!(tag, "undo" | "redo") {
            let history = match tag {
                "undo" => &self.undo,
                _ => &self.redo,
            };
            let Some(snapshot) = history.last() else {
                return wire::EditorDecision::Noop;
            };
            return wire::EditorDecision::Apply {
                patches: vec![wire::EditorPatch {
                    start_byte: 0,
                    end_byte: state.text.len() as u32,
                    replacement: snapshot.text.clone(),
                }],
                cursor: snapshot.cursor,
                history: match tag {
                    "undo" => wire::EditorHistoryEffect::Undo,
                    _ => wire::EditorHistoryEffect::Redo,
                },
            };
        }
        if tag == "restore" {
            let empty = state.text.is_empty() && self.attachments.is_empty();
            if !empty {
                return wire::EditorDecision::Noop;
            }
            let Some(send) = &self.failed_send else {
                return wire::EditorDecision::Noop;
            };
            let restored = Draft::from_body(&send.body, choices);
            let text = restored.editor.text();
            let caret = text.len();
            return apply(state, 0..state.text.len(), text, caret);
        }
        if tag == "send" {
            if !self.can_send(state.text) {
                return wire::EditorDecision::Noop;
            }
            return apply(state, 0..state.text.len(), String::new(), 0);
        }
        let range = self.expanded(selection(state.text, state.cursor), tag);
        if let Some((open, close)) = markers(tag) {
            let replacement = format!("{open}{}{close}", &state.text[range.clone()]);
            let caret = if range.is_empty() {
                range.start + open.len()
            } else {
                range.start + replacement.len()
            };
            return apply(state, range, replacement, caret);
        }
        if let Some(token) = tag.strip_prefix("mention:") {
            let Some(choice) = choices.iter().find(|choice| choice.token == token) else {
                return wire::EditorDecision::Noop;
            };
            let Some((range, _)) = self.query(state) else {
                return wire::EditorDecision::Noop;
            };
            let text = format!("@{} ", choice.label);
            let caret = range.start + text.len();
            return apply(state, range, text, caret);
        }
        let attachment_action = tag.starts_with("remove:") || tag.starts_with("retry:");
        if attachment_action {
            return wire::EditorDecision::Apply {
                patches: Vec::new(),
                cursor: state.cursor,
                history: wire::EditorHistoryEffect::Native,
            };
        }
        match tag {
            "paste-ready" => {
                let Some(body) = &self.paste else {
                    return wire::EditorDecision::Noop;
                };
                let pasted = Draft::from_body(body, choices);
                let text = pasted.editor.text();
                let caret = range.start + text.len();
                apply(state, range, text, caret)
            }
            "cut" | "backspace" | "delete" => {
                if range.is_empty() {
                    return wire::EditorDecision::DefaultEditorAction;
                }
                let caret = range.start;
                apply(state, range, String::new(), caret)
            }
            "attach" | "paste" | "copy" | "menu-next" | "menu-previous" | "menu-dismiss" => {
                wire::EditorDecision::Apply {
                    patches: Vec::new(),
                    cursor: state.cursor,
                    history: wire::EditorHistoryEffect::Native,
                }
            }
            _ => wire::EditorDecision::DefaultEditorAction,
        }
    }

    pub fn committed(
        &mut self,
        before: &str,
        after: &str,
        cursor: wire::EditorCursor,
        tag: &str,
        choices: &[MentionChoice],
    ) {
        if tag == "send" {
            if !self.can_send(before) {
                return;
            }
            self.submitted = Some(super::Send {
                body: self.body_of(before).trim().to_owned(),
                attachments: std::mem::take(&mut self.attachments),
            });
            self.mentions.clear();
            self.undo.clear();
            self.redo.clear();
            return;
        }
        let changed = before != after;
        if changed {
            let snapshot = super::History {
                text: before.into(),
                mentions: self.mentions.clone(),
                cursor,
            };
            match tag {
                "undo" => {
                    if let Some(previous) = self.undo.pop() {
                        self.mentions = previous.mentions;
                        self.redo.push(snapshot);
                    }
                    return;
                }
                "redo" => {
                    if let Some(next) = self.redo.pop() {
                        self.mentions = next.mentions;
                        self.undo.push(snapshot);
                    }
                    return;
                }
                _ => {
                    self.redo.clear();
                    self.undo.push(snapshot);
                    while self.undo.len() > 128
                        || self
                            .undo
                            .iter()
                            .map(|entry| entry.text.len())
                            .sum::<usize>()
                            > 8 << 20
                    {
                        self.undo.remove(0);
                    }
                }
            }
        }
        match tag {
            "menu-next" => {
                self.menu_index = self.menu_index.saturating_add(1);
                return;
            }
            "menu-previous" => {
                self.menu_index = self.menu_index.saturating_sub(1);
                return;
            }
            "menu-dismiss" => {
                self.menu_dismissed = true;
                return;
            }
            _ => {}
        }
        if tag == "restore" {
            let Some(send) = self.failed_send.take() else {
                return;
            };
            let restored = Draft::from_body(&send.body, choices);
            self.mentions = restored.mentions;
            self.attachments = send.attachments;
            return;
        }
        let range = self.expanded(selection(before, cursor), tag);
        if matches!(tag, "copy" | "cut") {
            let mut copied = String::new();
            let mut at = range.start;
            for mention in &self.mentions {
                let contained =
                    mention.range.start >= range.start && mention.range.end <= range.end;
                if contained {
                    copied.push_str(&before[at..mention.range.start]);
                    copied.push_str(&mention.token);
                    at = mention.range.end;
                }
            }
            copied.push_str(&before[at..range.end]);
            self.clipboard = Some(copied);
        }
        if tag == "paste-ready" {
            let Some(body) = self.paste.take() else {
                return;
            };
            let pasted = Draft::from_body(&body, choices);
            self.observed(before, after);
            self.mentions
                .extend(pasted.mentions.into_iter().map(|mut mention| {
                    mention.range.start += range.start;
                    mention.range.end += range.start;
                    mention
                }));
            self.mentions.sort_by_key(|mention| mention.range.start);
            return;
        }
        if let Some((open, close)) = markers(tag) {
            for mention in &mut self.mentions {
                if mention.range.start >= range.end {
                    mention.range.start += open.len() + close.len();
                    mention.range.end += open.len() + close.len();
                } else if mention.range.start >= range.start && mention.range.end <= range.end {
                    mention.range.start += open.len();
                    mention.range.end += open.len();
                }
            }
            return;
        }
        let picked = tag
            .strip_prefix("mention:")
            .and_then(|token| choices.iter().find(|choice| choice.token == token))
            .and_then(|choice| {
                let state = EditorStateView {
                    text: before,
                    cursor,
                    reset: 0,
                    text_revision: 0,
                    revision: 0,
                };
                let (range, _) = self.query(state)?;
                Some(Mention {
                    range: range.start..range.start + 1 + choice.label.len(),
                    token: choice.token.clone(),
                })
            });
        self.observed(before, after);
        if let Some(mention) = picked {
            self.mentions.push(mention);
            self.mentions.sort_by_key(|mention| mention.range.start);
        }
    }
}
