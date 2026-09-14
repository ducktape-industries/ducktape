//! The Pages presentation consumer: native-compatible comment badges and an
//! explicit plain-editor notice when the complete presentation cannot be used.
use crate::{document_sync::CommentMark, editor_binding::MenuState};
use ducktape_view_guest::{Editor, EditorStateView, wire};
use wire::editor_presentation::{EditorMargin, EditorPresentation, PresentationError};

/// The gap one line of the document holds open below itself, in whole pixels.
/// An inline comment card lives in it: the card is a stack layer over the
/// document, so the space it needs has to come out of the document's own
/// layout or the card would sit on top of the text it belongs to.
#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub struct EditorReserve {
    pub line: i64,
    pub height: i64,
}

fn prepare(
    state: EditorStateView<'_>,
    menu: MenuState,
    dark: bool,
    commented: Vec<i64>,
    marks: Vec<CommentMark>,
    reserve: EditorReserve,
) -> Result<EditorPresentation, PresentationError> {
    let mut paint = crate::presentation::build(state, menu, dark, commented, reserve)?;
    let lines = wire::editor_lines(state.text).count();
    paint.affordances.margin_label = "Open comments".into();
    // Saved anchors can lag unsaved line deletion. The native document also
    // hides badges outside its current line count; it never moves their target.
    paint.affordances.margins = marks
        .into_iter()
        .filter_map(|mark| {
            let line = u32::try_from(mark.line).ok()?;
            let count = u32::try_from(mark.count).ok()?;
            ((line as usize) < lines && count > 0).then_some(EditorMargin { line, count })
        })
        .collect();
    paint.validate(state.text)?;
    Ok(paint)
}

/// A bounded derived value, shared by the visible notice and highlighter. It
/// contains no second document text and is snapshotted with its canonical state.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedPresentation {
    reference: PresentationReference,
    pub data: Vec<u8>,
    pub notice: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PresentationReference {
    reset: u64,
    text_revision: u64,
    revision: u64,
    cursor: wire::EditorCursor,
}

impl PreparedPresentation {
    pub fn validate(&self, document: &Editor) -> Result<(), String> {
        if self.data.is_empty() {
            return Ok(());
        }
        if self.data.len() > 1024 * 1024 {
            return Err("Pages presentation snapshot exceeds limit".into());
        }
        let paint: EditorPresentation = wire::decode(&self.data)?;
        if self.reference == reference(document.state_view()) {
            paint
                .validate(document.state_view().text)
                .map_err(|error| format!("invalid Pages presentation snapshot: {error:?}"))?;
        }
        Ok(())
    }
}

pub fn empty_presentation() -> PreparedPresentation {
    PreparedPresentation::default()
}

pub fn no_reserve() -> EditorReserve {
    EditorReserve::default()
}

pub fn document_presentation(
    document: &Editor,
    menu: MenuState,
    dark: bool,
    commented: Vec<i64>,
    marks: Vec<CommentMark>,
    reserve: EditorReserve,
) -> PreparedPresentation {
    let state = document.state_view();
    let (paint, notice) = match prepare(state, menu, dark, commented, marks, reserve) {
        Ok(paint) => (paint, String::new()),
        Err(_) => (EditorPresentation::default(), FORMAT_NOTICE.into()),
    };
    let mut data = wire::encode(&paint);
    let mut notice = notice;
    if data.len() > 1024 * 1024 {
        data = wire::encode(&EditorPresentation::default());
        notice = FORMAT_NOTICE.into();
    }
    PreparedPresentation {
        reference: reference(state),
        data,
        notice,
    }
}

const FORMAT_NOTICE: &str =
    "Formatting is unavailable for this document. Your text and undo history are preserved.";
fn reference(state: EditorStateView<'_>) -> PresentationReference {
    PresentationReference {
        reset: state.reset,
        text_revision: state.text_revision,
        revision: state.revision,
        cursor: state.cursor,
    }
}

pub fn presentation_notice(document: &Editor, prepared: &PreparedPresentation) -> String {
    if prepared.reference != reference(document.state_view()) && !prepared.data.is_empty() {
        return FORMAT_NOTICE.into();
    }
    prepared.notice.clone()
}

pub fn paint(state: EditorStateView<'_>, prepared: &PreparedPresentation) -> EditorPresentation {
    if prepared.reference != reference(state) || prepared.data.is_empty() {
        return EditorPresentation::default();
    }
    wire::decode(&prepared.data).expect("prepared Pages presentation")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(document: &Editor) -> PreparedPresentation {
        document_presentation(
            document,
            crate::editor_binding::initial_menu(),
            false,
            Vec::new(),
            Vec::new(),
            EditorReserve::default(),
        )
    }

    #[test]
    fn typed_reference_roundtrips_and_rejects_each_stale_projection_dimension() {
        let document = Editor::new("Title\nBody");
        let cached = prepared(&document);
        let restored: PreparedPresentation = wire::decode(&wire::encode(&cached)).unwrap();
        assert_eq!(restored, cached);
        let current = document.state_view();
        assert!(!paint(current, &restored).spans.is_empty());
        let moved_cursor = wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 1 },
            selection: Some(Default::default()),
        };
        for changed in [
            EditorStateView {
                reset: current.reset + 1,
                ..current
            },
            EditorStateView {
                revision: current.revision + 1,
                ..current
            },
            EditorStateView {
                text_revision: current.text_revision + 1,
                ..current
            },
            EditorStateView {
                cursor: moved_cursor,
                ..current
            },
        ] {
            assert_eq!(paint(changed, &restored), EditorPresentation::default());
        }
        let mut replacement = document;
        let reset = replacement.reset_revision();
        replacement.replace(Editor::new("Replacement"), reset);
        assert_eq!(presentation_notice(&replacement, &restored), FORMAT_NOTICE);
    }

    #[test]
    fn bounded_fallback_keeps_the_document_and_explains_missing_formatting() {
        let text = "a".repeat(512 * 1024 + 1);
        let document = Editor::new(&text);
        let cached = prepared(&document);
        assert_eq!(document.state_view().text, text);
        assert_eq!(presentation_notice(&document, &cached), FORMAT_NOTICE);
        assert!(cached.data.len() <= 1024 * 1024);
        assert_eq!(
            paint(document.state_view(), &cached),
            EditorPresentation::default()
        );
    }
}
