//! Plain document state; the host retains its native editor between observations.
use crate::wire;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Editor(wire::EditorState, u64);
impl Editor {
    pub fn new(text: impl Into<String>) -> Self {
        let mut state = wire::EditorState {
            text: text.into(),
            ..Default::default()
        };
        assert!(
            state.text.len() <= wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES,
            "editor document exceeds text limit"
        );
        state.cursor.clamp(&state.text);
        Self(state, 0)
    }
    pub fn text(&self) -> String {
        self.0.text.clone()
    }
    pub(crate) fn text_ref(&self) -> &str {
        &self.0.text
    }
    /// Borrow the canonical editor state for deterministic presentation.
    pub fn state_view(&self) -> crate::EditorStateView<'_> {
        crate::EditorStateView {
            text: &self.0.text,
            cursor: self.0.cursor,
            reset: self.0.reset,
            text_revision: self.1,
            revision: self.0.revision,
        }
    }
    pub fn document_reference(&self, document: String) -> wire::editor_document::EditorDocumentRef {
        wire::editor_document::EditorDocumentRef {
            document,
            reset: self.0.reset,
            text_revision: self.1,
            revision: self.0.revision,
            cursor: self.0.cursor,
            byte_len: self.0.text.len() as u32,
        }
    }
    pub fn cursor(&self) -> wire::EditorCursor {
        self.0.cursor
    }
    pub fn observation_revision(&self) -> u64 {
        self.0.revision
    }
    pub fn reset_revision(&self) -> u64 {
        self.0.reset
    }
    pub fn line_count(&self) -> usize {
        wire::editor_lines(&self.0.text).count()
    }
    pub fn line(&self, line: usize) -> Option<String> {
        wire::editor_lines(&self.0.text)
            .nth(line)
            .map(str::to_owned)
    }
    /// An authoritative assignment, including an identical-text document replacement.
    pub fn replace(&mut self, mut next: Self, previous_reset: u64) {
        next.0.reset = previous_reset
            .checked_add(1)
            .expect("editor reset revisions exhausted");
        assert!(
            next.0.text.len() <= wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES,
            "editor document exceeds text limit"
        );
        next.0.cursor.clamp(&next.0.text);
        next.1 = 0;
        *self = next;
    }
    /// Observations from a previous document cannot overwrite a replacement.
    pub fn accept(&mut self, mut state: wire::EditorState) {
        if state.reset == self.0.reset
            && state.revision > self.0.revision
            && state.text.len() <= wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES
        {
            state.cursor.clamp(&state.text);
            if state.text != self.0.text {
                self.1 = self
                    .1
                    .checked_add(1)
                    .expect("editor text revisions exhausted");
            }
            self.0 = state;
        }
    }
    pub(crate) fn install_mirror(
        &mut self,
        text: String,
        target: &wire::editor_document::EditorDocumentRef,
    ) -> bool {
        if target.reset != self.0.reset
            || target.revision < self.0.revision
            || target.text_revision < self.1
            || target.validate_text(&text).is_err()
        {
            return false;
        }
        self.0.text = text;
        self.0.cursor = target.cursor;
        self.0.revision = target.revision;
        self.1 = target.text_revision;
        true
    }
    pub(crate) fn accept_patch(
        &mut self,
        before: &wire::editor_document::EditorDocumentRef,
        after: &wire::editor_document::EditorDocumentRef,
        patches: &[wire::EditorPatch],
    ) -> Option<Option<String>> {
        if &self.document_reference(before.document.clone()) != before
            || before.document != after.document
            || before.reset != after.reset
            || after.revision <= before.revision
        {
            return None;
        }
        if patches.is_empty() {
            if after.text_revision != before.text_revision
                || after.validate_text(&self.0.text).is_err()
            {
                return None;
            }
            self.0.cursor = after.cursor;
            self.0.revision = after.revision;
            return Some(None);
        }
        let text =
            wire::editor_transaction::patched_editor_text(&self.0.text, patches, after.cursor)
                .ok()?;
        let expected = before
            .text_revision
            .checked_add(u64::from(text != self.0.text))?;
        if after.text_revision != expected || after.validate_text(&text).is_err() {
            return None;
        }
        let old = std::mem::replace(&mut self.0.text, text);
        self.0.cursor = after.cursor;
        self.0.revision = after.revision;
        self.1 = after.text_revision;
        Some(Some(old))
    }
    pub fn move_to(&mut self, mut cursor: wire::EditorCursor) {
        cursor.clamp(&self.0.text);
        self.0.cursor = cursor;
        self.0.reset = self
            .0
            .reset
            .checked_add(1)
            .expect("editor reset revisions exhausted");
        self.1 = 0;
    }
    pub fn snapshot(&self) -> Vec<u8> {
        wire::encode(&(&self.0, self.1))
    }
    pub fn restore(bytes: &[u8]) -> Option<Self> {
        let (state, text_revision): (wire::EditorState, u64) = wire::decode(bytes).ok()?;
        if state.text.len() > wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES {
            return None;
        }
        let mut cursor = state.cursor;
        cursor.clamp(&state.text);
        (cursor == state.cursor).then_some(Self(state, text_revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_references_separate_text_revisions_from_caret_observations() {
        let mut editor = Editor::new("a");
        editor.accept(wire::EditorState {
            text: "ab".into(),
            revision: 4,
            ..Default::default()
        });
        let typed = editor.document_reference("app:draft".into());
        assert_eq!(
            (typed.text_revision, typed.revision, typed.byte_len),
            (1, 4, 2)
        );
        editor.accept(wire::EditorState {
            text: "ab".into(),
            revision: 5,
            ..Default::default()
        });
        let caret = editor.document_reference("app:draft".into());
        assert_eq!((caret.text_revision, caret.revision), (1, 5));
        assert_eq!(Editor::restore(&editor.snapshot()), Some(editor.clone()));
        editor.move_to(wire::EditorCursor::default());
        assert_eq!(
            editor.document_reference("app:draft".into()).text_revision,
            0
        );
    }

    #[test]
    fn oversized_initial_and_observed_documents_never_become_prefixes() {
        let oversized = "x".repeat(wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES + 1);
        assert!(std::panic::catch_unwind(|| Editor::new(oversized.clone())).is_err());
        let mut editor = Editor::new("preserved");
        editor.accept(wire::EditorState {
            text: oversized,
            revision: 1,
            ..Default::default()
        });
        assert_eq!(editor.text(), "preserved");
        assert_eq!(editor.observation_revision(), 0);
    }

    #[test]
    fn observations_do_not_reset_and_old_document_events_cannot_replace_new_state() {
        let mut editor = Editor::new("a");
        let observed = wire::EditorState {
            text: "한글".into(),
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 0, column: 6 },
                selection: Some(wire::EditorPosition { line: 0, column: 0 }),
            },
            reset: 0,
            revision: 100,
        };
        editor.accept(observed.clone());
        assert_eq!(editor.text(), "한글");
        assert_eq!(editor.reset_revision(), 0);
        assert_eq!(editor.cursor().selection.unwrap().column, 0);
        let mut stale = observed.clone();
        stale.revision = 99;
        stale.text = "old".into();
        editor.accept(stale);
        assert_eq!(editor.text(), "한글");
        assert_eq!(Editor::restore(&editor.snapshot()), Some(editor.clone()));
        let mut positioned = editor.clone();
        positioned.move_to(wire::EditorCursor::default());
        assert_eq!(positioned.reset_revision(), 1);
        positioned.accept(observed.clone());
        assert_eq!(
            positioned.cursor(),
            wire::EditorCursor::default(),
            "caret commands fence old observations and clear selection"
        );

        editor.replace(Editor::new("한글"), editor.reset_revision());
        assert_eq!(editor.reset_revision(), 1);
        assert_eq!(editor.cursor().selection, None);
        editor.accept(observed);
        assert_eq!(
            editor.cursor().position.column,
            0,
            "late old-document cursor stays rejected"
        );
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;

    #[test]
    fn same_text_replacement_preserves_the_verified_text_revision() {
        let mut editor = Editor::new("same");
        let before = editor.document_reference("app:doc".into());
        let mut after = before.clone();
        after.revision += 1;
        let patches = [wire::EditorPatch {
            start_byte: 0,
            end_byte: 4,
            replacement: "same".into(),
        }];
        let mut invalid = after.clone();
        invalid.text_revision += 1;
        assert!(
            editor.accept_patch(&before, &invalid, &patches).is_none(),
            "patch presence alone cannot advance the text revision"
        );
        assert_eq!(editor.document_reference("app:doc".into()), before);
        assert!(editor.accept_patch(&before, &after, &patches).is_some());
        assert_eq!(editor.document_reference("app:doc".into()), after);
    }
}
