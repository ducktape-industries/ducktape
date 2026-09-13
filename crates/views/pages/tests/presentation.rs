//! Compare the flattened wire paint with the existing native Markdown policy.
use ducktape_view_guest::{Editor, wire};
use pages_view::{editor_binding, editor_menu, editor_view, editor_view::EditorReserve, markdown};
#[path = "../src/editor_presentation.rs"]
pub mod presentation;

#[test]
fn flattened_runs_preserve_native_body_gaps_and_inline_precedence() {
    let text = "Title\n한글 ordinary **bold** gap _italic_ and https://example.com/end.\n- [ ] todo 👍🏽\n```\nlet n = 1;\n```";
    let editor = Editor::new(text);
    for dark in [false, true] {
        let caret = markdown::Caret {
            focused: true,
            line: 2,
            column: 0,
            dark,
            commented: vec![1],
        };
        let state = ducktape_view_guest::EditorStateView {
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 2, column: 0 },
                selection: None,
            },
            ..editor.state_view()
        };
        let actual = presentation::build(
            state,
            editor_binding::initial_menu(),
            dark,
            vec![1],
            true,
            EditorReserve::default(),
        )
        .unwrap();
        let mut native = markdown::DocumentHighlighter::new(&caret);
        for (line, source) in wire::editor_lines(text).enumerate() {
            let expected: Vec<_> = native.highlight_line(source).collect();
            for (byte, _) in source.char_indices() {
                let expected = expected
                    .iter()
                    .rev()
                    .find(|(range, _)| range.contains(&byte))
                    .map(|(_, mark)| markdown::format(mark, dark));
                let actual = actual
                    .spans
                    .iter()
                    .find(|span| {
                        span.line as usize == line
                            && span.start as usize <= byte
                            && byte < span.end as usize
                    })
                    .map(|span| actual.formats[span.format as usize].clone());
                assert_eq!(
                    actual, expected,
                    "line {line}, UTF-8 byte {byte}, dark {dark}"
                );
            }
        }
    }
}

#[test]
fn named_link_syntax_hides_on_blur_and_returns_without_changing_source_or_caret() {
    let line = "앞 [작업 보기](duck://agents/runs/abc) 뒤";
    let text = format!("Title\n{line}");
    let editor = Editor::new(&text);
    let state = ducktape_view_guest::EditorStateView {
        cursor: wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 8 },
            selection: None,
        },
        ..editor.state_view()
    };
    for focused in [true, false, true] {
        let paint = presentation::build(
            state,
            editor_binding::initial_menu(),
            false,
            vec![],
            focused,
            EditorReserve::default(),
        )
        .unwrap();
        let visible: String = paint
            .spans
            .iter()
            .filter(|span| span.line == 1)
            .filter(|span| paint.formats[span.format as usize].size.unwrap_or(14.0) > 1.0)
            .map(|span| &line[span.start as usize..span.end as usize])
            .collect();
        assert_eq!(
            visible,
            if focused {
                line
            } else {
                "앞 작업 보기 뒤"
            }
        );
        let hit = paint
            .affordances
            .hits
            .iter()
            .find(|hit| hit.tag == 2)
            .unwrap();
        assert_eq!(&line[hit.start as usize..hit.end as usize], "작업 보기");
        assert_eq!(
            pages_view::inline::document_link_at(line, hit.start as usize).as_deref(),
            Some("duck://agents/runs/abc")
        );
        assert_eq!(state.text, text);
        assert_eq!(state.cursor.position.column, 8);
    }
}

#[test]
fn named_links_use_markdown_destinations_and_leave_incomplete_syntax_editable() {
    use pages_view::inline::{Inline, document_link_at, document_marks};
    for (source, label, destination) in [
        (
            "[문서](https://example.com/a_(b))",
            "문서",
            "https://example.com/a_(b)",
        ),
        (
            "[문서](<https://example.com/a> \"설명\")",
            "문서",
            "https://example.com/a",
        ),
        (
            "[문서](https://example.com/?a=1&amp;b=2)",
            "문서",
            "https://example.com/?a=1&b=2",
        ),
    ] {
        let marks = document_marks(source);
        let (range, _) = marks
            .iter()
            .find(|(_, mark)| *mark == Inline::Link)
            .unwrap();
        assert_eq!(&source[range.clone()], label);
        assert_eq!(
            document_link_at(source, range.start).as_deref(),
            Some(destination)
        );
        assert_eq!(
            marks
                .iter()
                .filter(|(_, kind)| *kind == Inline::Marker)
                .count(),
            2
        );
    }
    for source in ["[문서](not finished", "\\[문서](https://example.com)"] {
        assert!(
            !document_marks(source)
                .iter()
                .any(|(_, mark)| *mark == Inline::Marker)
        );
    }
}

/// The gap an inline comment card sits in is the anchored line's own bottom
/// padding, plus the card. Nothing else on the page moves, and the line keeps
/// whatever indent it was already owed.
#[test]
fn the_reserved_line_carries_the_gap_and_keeps_its_own_padding() {
    let text = "Title\nordinary paragraph\n  - nested item\nanother paragraph";
    let editor = Editor::new(text);
    let state = editor.state_view();
    let bottom_of = |paint: &wire::editor_presentation::EditorPresentation, line: u32| {
        paint
            .spans
            .iter()
            .filter(|span| span.line == line)
            .map(|span| paint.formats[span.format as usize].line_padding)
            .rfind(|padding| *padding != wire::Edges::default())
            .unwrap_or_default()
    };
    let plain = presentation::build(
        state,
        editor_binding::initial_menu(),
        false,
        vec![],
        true,
        EditorReserve::default(),
    )
    .unwrap();
    // The nested line: an indent it must not lose to the reserve.
    assert!(bottom_of(&plain, 2).left > 0.0);
    let reserved = presentation::build(
        state,
        editor_binding::initial_menu(),
        false,
        vec![],
        true,
        EditorReserve {
            line: 2,
            height: 200,
        },
    )
    .unwrap();
    assert_eq!(
        bottom_of(&reserved, 2),
        wire::Edges {
            bottom: bottom_of(&plain, 2).bottom + 200.0,
            ..bottom_of(&plain, 2)
        }
    );
    for untouched in [0, 1, 3] {
        assert_eq!(
            bottom_of(&reserved, untouched),
            bottom_of(&plain, untouched),
            "line {untouched} is not the anchor"
        );
    }
    // A reserve of nothing is no reserve at all.
    let none = presentation::build(
        state,
        editor_binding::initial_menu(),
        false,
        vec![],
        true,
        EditorReserve { line: 2, height: 0 },
    )
    .unwrap();
    assert_eq!(none.formats, plain.formats);
    assert_eq!(none.spans, plain.spans);
    assert_eq!(editor_view::no_reserve(), EditorReserve::default());
}

/// The inline grammar past bold and italic: strike, code, highlight and a
/// member mention each paint their own run, and the markers around them hide
/// away from the caret line exactly as `**` does.
#[test]
fn strike_code_highlight_and_mentions_paint_their_own_runs() {
    use pages_view::inline::{Inline, inline_marks};
    let line = "a ~~gone~~ ++under++ `code` ==mark== @alice me@host.io done";
    let marks: Vec<_> = inline_marks(line)
        .into_iter()
        .filter(|(_, kind)| *kind != Inline::Marker)
        .map(|(range, kind)| (&line[range], kind))
        .collect();
    assert_eq!(
        marks,
        vec![
            ("gone", Inline::Strike),
            ("under", Inline::Underline),
            ("code", Inline::Code),
            ("mark", Inline::Highlight),
            ("@alice", Inline::Mention),
        ]
    );
    let text = format!("Title\n{line}");
    let editor = Editor::new(&text);
    let state = ducktape_view_guest::EditorStateView {
        cursor: wire::EditorCursor {
            position: wire::EditorPosition { line: 0, column: 0 },
            selection: None,
        },
        ..editor.state_view()
    };
    let paint = presentation::build(
        state,
        editor_binding::initial_menu(),
        false,
        vec![],
        true,
        EditorReserve::default(),
    )
    .unwrap();
    let format_of = |word: &str| {
        let start = line.find(word).unwrap() as u32;
        let span = paint
            .spans
            .iter()
            .find(|span| span.line == 1 && span.start <= start && start < span.end)
            .unwrap_or_else(|| panic!("no run covers {word:?}"));
        paint.formats[span.format as usize].clone()
    };
    assert!(format_of("gone").strikethrough.is_some());
    assert!(format_of("gone").underline.is_none());
    assert!(format_of("under").underline.is_some());
    assert!(format_of("under").strikethrough.is_none());
    assert!(format_of("code").background.is_some());
    assert!(format_of("mark").background.is_some());
    assert_ne!(format_of("@alice").color, format_of("done").color);
    let visible: String = paint
        .spans
        .iter()
        .filter(|span| span.line == 1)
        .filter(|span| paint.formats[span.format as usize].size.unwrap_or(14.0) > 1.0)
        .map(|span| &line[span.start as usize..span.end as usize])
        .collect();
    assert_eq!(visible, "a gone under code mark @alice me@host.io done");
}
