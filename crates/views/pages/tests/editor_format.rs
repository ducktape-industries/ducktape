//! The inline formatting edits behind the shortcuts and the floating menu:
//! wrap and unwrap around a selection or the word under the caret, links,
//! and the "reset formatting" strip — each one atomic, each its own undo step.
use pages_view::editor::{
    self, Doc, EditorCursor, EditorDecision, EditorHistoryEffect, EditorPosition,
};
use pages_view::format::{self, Wrap};
use pages_view::inline::{Inline, inline_marks};

fn doc(text: &str, line: usize, column: usize) -> Doc {
    Doc::new(text, EditorCursor::at(line, column))
}

fn selected(text: &str, line: usize, from: usize, to: usize) -> Doc {
    let mut doc = doc(text, line, to);
    doc.cursor.selection = Some(EditorPosition::new(line, from));
    doc
}

fn apply(before: &Doc, decision: EditorDecision) -> Doc {
    let EditorDecision::Apply {
        patches,
        cursor,
        history,
    } = decision
    else {
        panic!("a format edit must propose an atomic Apply");
    };
    assert_eq!(history, EditorHistoryEffect::NewGroup);
    editor::apply(before, &patches, cursor)
}

#[test]
fn bold_wraps_the_selection_and_keeps_it_selected_for_the_next_mark() {
    let before = selected("Title\nsome words here", 1, 5, 10);
    let after = apply(&before, format::toggle(&before, Wrap::Bold));
    assert_eq!(after.text, "Title\nsome **words** here");
    assert_eq!(after.cursor, {
        let mut cursor = EditorCursor::at(1, 12);
        cursor.selection = Some(EditorPosition::new(1, 7));
        cursor
    });
    let italic = apply(&after, format::toggle(&after, Wrap::Italic));
    assert_eq!(italic.text, "Title\nsome ***words*** here");
}

#[test]
fn a_second_toggle_unwraps_from_inside_or_around_the_selection() {
    let inside = selected("Title\nsome **words** here", 1, 5, 14);
    let plain = apply(&inside, format::toggle(&inside, Wrap::Bold));
    assert_eq!(plain.text, "Title\nsome words here");
    assert_eq!(plain.cursor.selection, Some(EditorPosition::new(1, 5)));
    assert_eq!(plain.cursor.position.column, 10);
    let around = selected("Title\nsome **words** here", 1, 7, 12);
    assert_eq!(
        apply(&around, format::toggle(&around, Wrap::Bold)).text,
        "Title\nsome words here"
    );
}

#[test]
fn without_a_selection_the_word_under_the_caret_takes_the_mark() {
    let mid_word = doc("Title\n한글 words here", 1, 9);
    let after = apply(&mid_word, format::toggle(&mid_word, Wrap::Strike));
    assert_eq!(after.text, "Title\n한글 ~~words~~ here");
    let between = doc("Title\nsome  here", 1, 5);
    let after = apply(&between, format::toggle(&between, Wrap::Code));
    assert_eq!(after.text, "Title\nsome `` here");
    assert_eq!(
        after.cursor.position.column, 6,
        "the caret waits inside the empty pair"
    );
}

#[test]
fn every_wrap_round_trips_through_the_inline_grammar() {
    for wrap in [
        Wrap::Bold,
        Wrap::Italic,
        Wrap::Strike,
        Wrap::Underline,
        Wrap::Code,
        Wrap::Highlight,
    ] {
        let before = selected("Title\nab cd ef", 1, 3, 5);
        let after = apply(&before, format::toggle(&before, wrap));
        let line = after.line_text(1);
        let body = inline_marks(&line)
            .into_iter()
            .find(|(_, kind)| *kind != Inline::Marker)
            .map(|(range, kind)| (line[range].to_owned(), kind))
            .expect("the wrapped run is a mark");
        assert_eq!(body.0, "cd", "{wrap:?}");
        assert_ne!(body.1, Inline::Link);
        let back = apply(&after, format::toggle(&after, wrap));
        assert_eq!(back.text, before.text, "{wrap:?}");
    }
}

/// Tiptap's colour mark: the span it serializes to wraps the target, a second
/// colour re-tints that span in place, and Default strips it.
#[test]
fn a_colour_wraps_retints_and_strips_around_the_target() {
    let before = selected("Title\nsome words here", 1, 5, 10);
    let red = apply(&before, format::color(&before, Some(0xd44c47)));
    assert_eq!(
        red.text,
        "Title\nsome <span style=\"color:#d44c47\">words</span> here"
    );
    assert_eq!(red.cursor.selection, Some(EditorPosition::new(1, 33)));
    assert_eq!(red.cursor.position.column, 38);
    let line = red.line_text(1);
    let marks: Vec<_> = inline_marks(&line)
        .into_iter()
        .map(|(range, kind)| (line[range].to_owned(), kind))
        .collect();
    assert_eq!(
        marks[1],
        ("words".to_owned(), Inline::Color(0xd44c47)),
        "{marks:?}"
    );
    assert_eq!(marks[0].1, Inline::Marker);
    assert_eq!(marks[2].1, Inline::Marker);
    // The caret alone inside the span is enough to re-tint the whole run.
    let inside = doc(&red.text, 1, 34);
    let blue = apply(&inside, format::color(&inside, Some(0x337ea9)));
    assert_eq!(
        blue.text,
        "Title\nsome <span style=\"color:#337ea9\">words</span> here"
    );
    let plain = apply(&blue, format::color(&blue, None));
    assert_eq!(plain.text, before.text);
    assert_eq!(plain.cursor, before.cursor);
    assert_eq!(
        format::color(&doc("Title\nbody", 0, 2), Some(0xd44c47)),
        EditorDecision::Noop,
        "the title takes no colour"
    );
}

#[test]
fn the_title_and_a_multi_line_selection_take_no_marks() {
    let title = selected("Title\nbody", 0, 0, 5);
    assert_eq!(format::toggle(&title, Wrap::Bold), EditorDecision::Noop);
    let mut across = doc("Title\none\ntwo", 2, 1);
    across.cursor.selection = Some(EditorPosition::new(1, 1));
    assert_eq!(format::toggle(&across, Wrap::Bold), EditorDecision::Noop);
    assert_eq!(format::link(&title), EditorDecision::Noop);
}

#[test]
fn a_link_names_the_selection_and_selects_the_url_slot() {
    let before = selected("Title\nsee the docs now", 1, 8, 12);
    let after = apply(&before, format::link(&before));
    assert_eq!(after.text, "Title\nsee the [docs](url) now");
    let start = after.cursor.selection.unwrap();
    assert_eq!(
        &after.line_text(1)[start.column as usize..after.cursor.position.column as usize],
        "url"
    );
    assert_eq!(
        format::link(&after),
        EditorDecision::Noop,
        "a named link is not named twice"
    );
}

#[test]
fn unlink_keeps_the_label_and_a_bare_url_is_left_alone() {
    let before = doc("Title\nsee the [docs](https://x.y) now", 1, 10);
    let after = apply(&before, format::unlink(&before, 1, 10));
    assert_eq!(after.text, "Title\nsee the docs now");
    assert_eq!(after.cursor, EditorCursor::at(1, 12));
    let bare = doc("Title\nsee https://x.y now", 1, 6);
    assert_eq!(format::unlink(&bare, 1, 6), EditorDecision::Noop);
}

#[test]
fn clear_strips_every_inline_marker_and_keeps_the_block_prefix() {
    let before = doc(
        "Title\n- **bold** and _it_ and `c` and ==m== and [l](u) 끝",
        1,
        20,
    );
    let after = apply(&before, format::clear(&before, 1));
    assert_eq!(after.text, "Title\n- bold and it and c and m and l 끝");
    assert!(after.text.is_char_boundary(after.offset_of_cursor()));
    assert_eq!(format::clear(&after, 1), EditorDecision::Noop);
}

trait LineText {
    fn line_text(&self, line: usize) -> String;
    fn offset_of_cursor(&self) -> usize;
}
impl LineText for Doc {
    fn line_text(&self, line: usize) -> String {
        self.text
            .split('\n')
            .nth(line)
            .unwrap_or_default()
            .to_owned()
    }
    fn offset_of_cursor(&self) -> usize {
        let line = self.cursor.position.line as usize;
        let before: usize = self.text.split('\n').take(line).map(|l| l.len() + 1).sum();
        before + self.cursor.position.column as usize
    }
}

#[test]
fn alignment_is_a_marker_after_the_prefix_and_the_caret_keeps_its_place() {
    use pages_view::markdown::Align;
    let before = doc("Title\n- item", 1, 6);
    let centered = apply(&before, format::align(&before, Align::Center));
    assert_eq!(centered, doc("Title\n- -> item", 1, 9));
    let right = apply(&centered, format::align(&centered, Align::End));
    assert_eq!(right, doc("Title\n- ->> item", 1, 10));
    let back = apply(&right, format::align(&right, Align::Start));
    assert_eq!(back, before);
    assert_eq!(format::align(&back, Align::Start), EditorDecision::Noop);
    // A caret inside the marker lands after the new one; one before it stays.
    let inside = doc("Title\n- ->> item", 1, 4);
    assert_eq!(
        apply(&inside, format::align(&inside, Align::Center)),
        doc("Title\n- -> item", 1, 5)
    );
    let ahead = doc("Title\n- ->> item", 1, 1);
    assert_eq!(
        apply(&ahead, format::align(&ahead, Align::Start)),
        doc("Title\n- item", 1, 1)
    );
}

#[test]
fn alignment_covers_every_selected_line_and_skips_the_title_fences_and_dividers() {
    use pages_view::markdown::Align;
    let text = "Title\none\n```\ncode\n```\n---\n> two";
    let mut before = doc(text, 6, 5);
    before.cursor.selection = Some(EditorPosition::new(0, 0));
    let after = apply(&before, format::align(&before, Align::Center));
    assert_eq!(after.text, "Title\n-> one\n```\ncode\n```\n---\n> -> two");
    assert_eq!(after.cursor.position, EditorPosition::new(6, 8));
    assert_eq!(after.cursor.selection, Some(EditorPosition::new(0, 0)));
    let title = doc(text, 0, 2);
    assert_eq!(format::align(&title, Align::End), EditorDecision::Noop);
    assert_eq!(
        apply(&after, format::align_lines(&after, 1..2, Align::Start)).text,
        "Title\none\n```\ncode\n```\n---\n> -> two"
    );
}
