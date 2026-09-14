//! The page editor's keys, held to the app's own fixtures (`app/src/pages`
//! tests) over the guest's document model: the text after one key is the
//! same, the caret lands where the app put it, and every answer is patches
//! against the document the key was pressed on.

use pages_view::editor::{
    Doc, EditorCursor, EditorDecision, EditorHistoryEffect, EditorPatch, EditorPosition, History,
    Key, MAX_PATCHES, apply, decide,
};

fn typed(text: &str, line: usize, column: usize) -> Doc {
    Doc::new(text, EditorCursor::at(line, column))
}

/// The document after one key, through the decision the guest gives.
fn pressed(doc: Doc, key: Key) -> Doc {
    match decide(&doc, key, &History::default(), 0) {
        EditorDecision::Apply {
            patches, cursor, ..
        } => {
            check_patches(&doc, &patches);
            apply(&doc, &patches, cursor)
        }
        EditorDecision::Noop => doc,
        EditorDecision::DefaultEditorAction => {
            panic!("the editor's default for {key:?} on {doc:?}")
        }
    }
}

/// The buffer's exact text after one key. NOT trimmed: a carried list
/// marker ends in the space the caret sits after.
fn press(doc: Doc, key: Key) -> String {
    pressed(doc, key).text
}

fn is_default(doc: Doc, key: Key) -> bool {
    decide(&doc, key, &History::default(), 0) == EditorDecision::DefaultEditorAction
}

/// Sorted, disjoint, on char boundaries of the document they are against.
fn check_patches(doc: &Doc, patches: &[EditorPatch]) {
    assert!(patches.len() <= MAX_PATCHES, "{} patches", patches.len());
    let mut at = 0;
    for patch in patches {
        let (start, end) = (patch.start_byte as usize, patch.end_byte as usize);
        assert!(at <= start && start <= end, "out of order: {patches:?}");
        assert!(
            doc.text.is_char_boundary(start) && doc.text.is_char_boundary(end),
            "mid-codepoint: {patch:?}"
        );
        at = end;
    }
}

#[test]
fn enter_carries_a_bullet_down() {
    assert_eq!(press(typed("- one", 0, 5), Key::Enter), "- one\n- ");
}

#[test]
fn enter_increments_an_ordered_marker() {
    assert_eq!(
        press(typed("1. one\n2. two", 1, 6), Key::Enter),
        "1. one\n2. two\n3. "
    );
}

#[test]
fn enter_renumbers_the_ordered_run_below_the_new_item() {
    let doc = typed("1. one\n2. two\n3. three", 0, 6);
    let EditorDecision::Apply {
        patches, cursor, ..
    } = decide(&doc, Key::Enter, &History::default(), 0)
    else {
        panic!("applied");
    };
    // The new item and each moved number: one patch per changed line.
    assert_eq!(patches.len(), 3, "{patches:?}");
    assert_eq!(
        apply(&doc, &patches, cursor).text,
        "1. one\n2. \n3. two\n4. three"
    );
    assert_eq!(cursor, EditorCursor::at(1, 3));
}

#[test]
fn renumbering_steps_over_children_and_stops_at_the_run_s_end() {
    // A deeper line belongs to the item above it and counts on its own;
    // the paragraph ends the run, so what follows is a NEW list at 1.
    let nested = typed("1. one\n  1. child\n2. two\nplain\n9. apart", 0, 6);
    assert_eq!(
        press(nested, Key::Enter),
        "1. one\n2. \n  1. child\n3. two\nplain\n1. apart"
    );
}

#[test]
fn a_bullet_run_below_is_left_alone() {
    assert_eq!(
        press(typed("- one\n- two", 0, 5), Key::Enter),
        "- one\n- \n- two"
    );
}

#[test]
fn backspacing_a_marker_out_restarts_the_run_below_it() {
    // "two" stops being an item, so what is left below it is a NEW list.
    assert_eq!(
        press(typed("1. one\n2. two\n3. three", 1, 3), Key::Backspace),
        "1. one\ntwo\n1. three"
    );
}

#[test]
fn tab_recounts_both_the_run_it_left_and_the_one_it_joined() {
    // Line 0 is the title, so the list starts on line 1. "two" nests under
    // "one" as its first child, and "three" takes the number "two" gave up.
    assert_eq!(
        press(typed("Title\n1. one\n2. two\n3. three", 2, 6), Key::Tab),
        "Title\n1. one\n  1. two\n2. three"
    );
}

#[test]
fn shift_tab_lifts_an_item_back_into_the_run_above_it() {
    assert_eq!(
        press(typed("1. one\n  1. two\n  2. three", 2, 9), Key::ShiftTab),
        "1. one\n  1. two\n2. three"
    );
}

#[test]
fn backspace_before_the_marker_drops_it_and_recounts_what_is_left() {
    // Column 0 sits before a marker the row shows raw: the marker goes, the
    // line above stays, and the run below restarts from one.
    let dropped = pressed(typed("1. one\n2. two\n3. three", 1, 0), Key::Backspace);
    assert_eq!(dropped.text, "1. one\ntwo\n1. three");
    assert_eq!(dropped.cursor, EditorCursor::at(1, 0));
}

#[test]
fn a_run_is_counted_from_its_own_start_not_from_the_number_above() {
    // Typing "5." does not buy a list that starts at five: the store keeps
    // no number, so the buffer has to show the position it will come back as.
    assert_eq!(
        press(typed("5. one\n9. two", 0, 6), Key::Enter),
        "1. one\n2. \n3. two"
    );
}

#[test]
fn enter_on_an_empty_item_ends_the_list() {
    let ended = pressed(typed("- one\n- ", 1, 2), Key::Enter);
    assert_eq!(ended.text, "- one\n");
    assert_eq!(ended.cursor, EditorCursor::at(1, 0));
}

#[test]
fn a_task_carries_down_unticked() {
    assert_eq!(
        press(typed("- [x] done", 0, 10), Key::Enter),
        "- [x] done\n- [ ] "
    );
}

#[test]
fn enter_outside_a_list_is_an_ordinary_newline() {
    assert!(is_default(typed("plain", 0, 5), Key::Enter));
}

#[test]
fn enter_on_an_open_fence_closes_it_with_the_caret_inside() {
    // Line 0 is the title, so the fence sits on line 1.
    let after = pressed(typed("Title\n```", 1, 3), Key::Enter);
    assert_eq!(after.text, "Title\n```\n\n```");
    // The caret parks on the blank line between the fences.
    assert_eq!(after.cursor, EditorCursor::at(2, 0));
}

#[test]
fn enter_on_a_closing_fence_is_an_ordinary_newline() {
    assert!(is_default(typed("Title\n```\ncode\n```", 3, 3), Key::Enter));
}

#[test]
fn a_nested_open_fence_closes_at_its_own_indent() {
    let after = pressed(typed("Title\n  ```", 1, 5), Key::Enter);
    assert_eq!(after.text, "Title\n  ```\n\n  ```");
    assert_eq!(after.cursor.position.column, 2);
}

#[test]
fn backspace_at_the_content_edge_drops_the_marker_not_the_line_above() {
    let dropped = pressed(typed("one\n- two", 1, 2), Key::Backspace);
    assert_eq!(dropped.text, "one\ntwo");
    assert_eq!(dropped.cursor, EditorCursor::at(1, 0));
    // A heading, a quote, a todo: the block's shape goes first, the line
    // turns into a paragraph, and only the next press joins the line above.
    let heading = pressed(typed("one\n## two", 1, 3), Key::Backspace);
    assert_eq!(heading.text, "one\ntwo");
    assert_eq!(heading.cursor, EditorCursor::at(1, 0));
    // A caret can still land before the collapsed `## ` (an arrow hop lands
    // on column 0): Backspace there drops the shape too, never the line above.
    let before_prefix = pressed(typed("one\n## two", 1, 0), Key::Backspace);
    assert_eq!(before_prefix.text, "one\ntwo");
    assert_eq!(before_prefix.cursor, EditorCursor::at(1, 0));
    let quote = pressed(typed("one\n> two", 1, 2), Key::Backspace);
    assert_eq!(quote.text, "one\ntwo");
    let todo = pressed(typed("one\n- [ ] two", 1, 6), Key::Backspace);
    assert_eq!(todo.text, "one\ntwo");
    assert!(
        is_default(typed("one\ntwo", 1, 0), Key::Backspace),
        "a paragraph's edge is the editor's own join with the line above"
    );
}

#[test]
fn backspace_inside_the_text_is_an_ordinary_delete() {
    assert!(is_default(typed("- two", 0, 5), Key::Backspace));
}

#[test]
fn tab_nests_by_the_projection_s_own_two_space_step() {
    assert_eq!(
        press(typed("Title\n- a\n- one", 2, 5), Key::Tab),
        "Title\n- a\n  - one"
    );
}

#[test]
fn tab_refuses_a_depth_the_tree_cannot_hold() {
    let refused = |doc: Doc| decide(&doc, Key::Tab, &History::default(), 0);
    // The first body line has no sibling to move under…
    assert_eq!(refused(typed("Title\n- one", 1, 5)), EditorDecision::Noop);
    // …and no line may go more than one step past the line above it.
    assert_eq!(
        refused(typed("Title\n- a\n  - b", 2, 7)),
        EditorDecision::Noop
    );
    assert_eq!(
        press(typed("Title\n- a\n- b", 2, 5), Key::Tab),
        "Title\n- a\n  - b"
    );
}

#[test]
fn shift_tab_lifts_one_step_and_stops_at_the_left_margin() {
    assert_eq!(press(typed("    - one", 0, 9), Key::ShiftTab), "  - one");
    assert_eq!(
        decide(&typed("- one", 0, 5), Key::ShiftTab, &History::default(), 0),
        EditorDecision::Noop
    );
}

#[test]
fn tab_keeps_the_caret_on_its_character() {
    let nested = pressed(typed("Title\n- a\n- one", 2, 5), Key::Tab);
    assert_eq!(nested.text, "Title\n- a\n  - one");
    assert_eq!(nested.cursor.position.column, 7);
    let lifted = pressed(typed("Title\n- a\n  - one", 2, 7), Key::ShiftTab);
    assert_eq!(lifted.text, "Title\n- a\n- one");
    assert_eq!(lifted.cursor.position.column, 5);
    // A caret inside the indent being removed clamps to the margin.
    let clamped = pressed(typed("Title\n- a\n  - one", 2, 1), Key::ShiftTab);
    assert_eq!(clamped.text, "Title\n- a\n- one");
    assert_eq!(clamped.cursor.position.column, 0);
}

#[test]
fn columns_are_utf8_bytes_and_land_on_char_boundaries() {
    // "- 한글" is 2 + 6 bytes.
    let carried = pressed(typed("- 한글", 0, 8), Key::Enter);
    assert_eq!(carried.text, "- 한글\n- ");
    assert_eq!(carried.cursor, EditorCursor::at(1, 2));
    let nested = pressed(typed("Title\n- a\n- 한글", 2, 8), Key::Tab);
    assert_eq!(nested.text, "Title\n- a\n  - 한글");
    assert_eq!(nested.cursor, EditorCursor::at(2, 10));
    // "- 👍🏽" is 2 + 8 bytes; a caret inside the removed indent clamps to
    // the margin, never into the emoji.
    let clamped = pressed(typed("Title\n- a\n  - 👍🏽", 2, 1), Key::ShiftTab);
    assert_eq!(clamped.text, "Title\n- a\n- 👍🏽");
    assert_eq!(clamped.cursor, EditorCursor::at(2, 0));
    let line = clamped.text.split('\n').nth(2).unwrap();
    assert!(line.is_char_boundary(clamped.cursor.position.column as usize));
}

#[test]
fn a_selection_is_the_editor_s_own_business() {
    // The transforms decline a selection; with nothing to recount below, the
    // editor's default (replace the selection) stands.
    let selected = Doc::new(
        "- one\n- two",
        EditorCursor {
            position: EditorPosition::new(1, 3),
            selection: Some(EditorPosition::new(0, 3)),
        },
    );
    assert!(is_default(selected.clone(), Key::Enter));
    assert!(is_default(selected, Key::Backspace));
}

#[test]
fn a_long_run_is_one_patch_per_changed_line_until_the_cap_folds_it() {
    let list = |items: usize| {
        (1..=items)
            .map(|n| format!("{n}. item"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // 250 items: the new item plus 249 moved numbers stay under the cap.
    let doc = typed(&list(250), 0, 7);
    let EditorDecision::Apply {
        patches, cursor, ..
    } = decide(&doc, Key::Enter, &History::default(), 0)
    else {
        panic!("applied");
    };
    check_patches(&doc, &patches);
    assert_eq!(patches.len(), 250);
    let recounted = apply(&doc, &patches, cursor).text;
    assert_eq!(recounted.lines().count(), 251);
    assert_eq!(recounted.lines().nth(1), Some("2. "));
    assert_eq!(recounted.lines().nth(2), Some("3. item"));
    assert_eq!(recounted.lines().last(), Some("251. item"));
    // 300 items would be 300 patches: the run folds into one span that
    // applies to the same text.
    let doc = typed(&list(300), 0, 7);
    let EditorDecision::Apply {
        patches, cursor, ..
    } = decide(&doc, Key::Enter, &History::default(), 0)
    else {
        panic!("applied");
    };
    check_patches(&doc, &patches);
    assert_eq!(patches.len(), 1);
    let folded = apply(&doc, &patches, cursor).text;
    assert_eq!(folded.lines().count(), 301);
    assert_eq!(folded.lines().last(), Some("301. item"));
}

// ---------- history ----------

#[test]
fn cmd_z_walks_the_history_and_shift_redoes() {
    let mut history = History::default();
    let before = typed("Title\nbod", 1, 3);
    // 'y' is a native edit: the host commits it, the guest groups it.
    let typed_doc = typed("Title\nbody", 1, 4);
    history.commit(&before, &typed_doc, EditorHistoryEffect::Native, 0);
    let EditorDecision::Apply {
        patches,
        cursor,
        history: effect,
    } = history.undo(&typed_doc).expect("an undo step")
    else {
        panic!("undo applies");
    };
    assert_eq!(effect, EditorHistoryEffect::Undo);
    check_patches(&typed_doc, &patches);
    let undone = apply(&typed_doc, &patches, cursor);
    assert_eq!(undone.text, "Title\nbod");
    // The caret returns to where the group STARTED, not to the origin.
    assert_eq!(undone.cursor, EditorCursor::at(1, 3));
    history.commit(&typed_doc, &undone, EditorHistoryEffect::Undo, 1);
    let EditorDecision::Apply {
        patches,
        cursor,
        history: effect,
    } = history.redo(&undone).expect("a redo step")
    else {
        panic!("redo applies");
    };
    assert_eq!(effect, EditorHistoryEffect::Redo);
    let redone = apply(&undone, &patches, cursor);
    assert_eq!(redone.text, "Title\nbody");
    // …and redo puts it back where the caret sat when Cmd+Z was pressed.
    assert_eq!(redone.cursor, EditorCursor::at(1, 4));
    history.commit(&undone, &redone, EditorHistoryEffect::Redo, 2);
    assert!(history.redo(&redone).is_none());
}

#[test]
fn keystrokes_inside_the_window_coalesce_into_one_step() {
    let mut history = History::default();
    history.commit(
        &typed("a", 0, 1),
        &typed("ab", 0, 2),
        EditorHistoryEffect::Native,
        0,
    );
    assert_eq!(
        history.group_effect(500),
        EditorHistoryEffect::ExtendPrevious
    );
    history.commit(
        &typed("ab", 0, 2),
        &typed("abc", 0, 3),
        EditorHistoryEffect::Native,
        500,
    );
    let undone = history.undo(&typed("abc", 0, 3)).expect("one step");
    let EditorDecision::Apply {
        patches, cursor, ..
    } = undone
    else {
        panic!("applies");
    };
    assert_eq!(apply(&typed("abc", 0, 3), &patches, cursor).text, "a");
    history.commit(
        &typed("abc", 0, 3),
        &typed("a", 0, 1),
        EditorHistoryEffect::Undo,
        600,
    );
    assert!(
        history.undo(&typed("a", 0, 1)).is_none(),
        "one group, one step"
    );
}

#[test]
fn a_fresh_edit_clears_the_redo_lane() {
    let mut history = History::default();
    history.commit(
        &typed("a", 0, 1),
        &typed("ab", 0, 2),
        EditorHistoryEffect::Native,
        0,
    );
    let _ = history.undo(&typed("ab", 0, 2));
    history.commit(
        &typed("ab", 0, 2),
        &typed("a", 0, 1),
        EditorHistoryEffect::Undo,
        1,
    );
    assert_eq!(history.group_effect(100), EditorHistoryEffect::NewGroup);
    history.commit(
        &typed("a", 0, 1),
        &typed("aX", 0, 2),
        EditorHistoryEffect::Native,
        100,
    );
    assert!(
        history.redo(&typed("aX", 0, 2)).is_none(),
        "redo dies on a fresh edit"
    );
}

#[test]
fn an_applied_decision_joins_the_open_group_it_named() {
    let mut history = History::default();
    let doc = typed("- one", 0, 5);
    // a native step from "- one" opens the group at t=0
    history.commit(&doc, &typed("- onex", 0, 6), EditorHistoryEffect::Native, 0);
    let EditorDecision::Apply {
        patches,
        cursor,
        history: effect,
    } = decide(&doc, Key::Enter, &history, 200)
    else {
        panic!("applies");
    };
    assert_eq!(effect, EditorHistoryEffect::ExtendPrevious);
    let after = apply(&doc, &patches, cursor);
    history.commit(&doc, &after, effect, 200);
    // One group: the undo lands before the whole burst.
    let EditorDecision::Apply {
        patches, cursor, ..
    } = history.undo(&after).expect("a step")
    else {
        panic!("applies");
    };
    assert_eq!(apply(&after, &patches, cursor).text, "- one");
    history.commit(&after, &doc, EditorHistoryEffect::Undo, 201);
    assert!(history.undo(&doc).is_none());
    // Outside the window the decision opens its own group.
    assert_eq!(
        decide(&typed("- one", 0, 5), Key::Enter, &history, 5_000),
        decide(&typed("- one", 0, 5), Key::Enter, &History::default(), 0)
    );
}

#[test]
fn a_restore_across_multibyte_text_stays_on_char_boundaries() {
    let mut history = History::default();
    let before = typed("Title\n한글", 1, 6);
    let after = typed("Title\n한국", 1, 6);
    history.commit(&before, &after, EditorHistoryEffect::Native, 0);
    let EditorDecision::Apply {
        patches, cursor, ..
    } = history.undo(&after).expect("a step")
    else {
        panic!("applies");
    };
    check_patches(&after, &patches);
    assert_eq!(apply(&after, &patches, cursor).text, "Title\n한글");
}

/// The lane acks a `Noop` as a commit whose text did not move, and caret
/// observations come the same way: neither is an undo step, and neither
/// kills the redo lane an undo just filled.
#[test]
fn a_caret_only_commit_after_undo_keeps_the_redo_lane() {
    let mut history = History::default();
    history.commit(
        &typed("a", 0, 1),
        &typed("ab", 0, 2),
        EditorHistoryEffect::Native,
        0,
    );
    let _ = history.undo(&typed("ab", 0, 2));
    history.commit(
        &typed("ab", 0, 2),
        &typed("a", 0, 1),
        EditorHistoryEffect::Undo,
        1,
    );
    // a Noop ack, then a caret move: same text, other caret
    history.commit(
        &typed("a", 0, 1),
        &typed("a", 0, 1),
        EditorHistoryEffect::Native,
        100,
    );
    history.commit(
        &typed("a", 0, 1),
        &typed("a", 0, 0),
        EditorHistoryEffect::Native,
        200,
    );
    assert_eq!(
        history.group_effect(250),
        EditorHistoryEffect::NewGroup,
        "no group opened"
    );
    let redone = history.redo(&typed("a", 0, 0)).expect("redo survives");
    let EditorDecision::Apply { cursor, .. } = redone else {
        panic!("redo applies");
    };
    assert_eq!(cursor, EditorCursor::at(0, 2));
}

#[test]
fn a_cancelled_undo_decision_does_not_consume_its_history_step() {
    let mut history = History::default();
    let before = typed("- 한글", 0, 8);
    let after = typed("- 한글!", 0, 9);
    history.commit(&before, &after, EditorHistoryEffect::Native, 0);
    let proposed = history.undo(&after).expect("one undo step");
    // The host cancelled the proposed transaction; no commit arrived.
    assert_eq!(history.undo(&after), Some(proposed));
    assert!(
        history.redo(&before).is_none(),
        "an uncommitted undo creates no redo"
    );
}

#[test]
fn moving_a_large_current_document_to_redo_evicts_oldest_history() {
    let mut history = History::default();
    let a = typed(&"a".repeat(900 * 1024), 0, 0);
    let b = typed(&"b".repeat(900 * 1024), 0, 0);
    let c = typed(&"c".repeat(100 * 1024), 0, 0);
    let d = typed(&"d".repeat(1024 * 1024), 0, 0);
    for (before, after) in [(&a, &b), (&b, &c), (&c, &d)] {
        history.commit(before, after, EditorHistoryEffect::NewGroup, 0);
    }
    assert!(history.undo(&d).is_some());
    history.commit(&d, &c, EditorHistoryEffect::Undo, 1);
    assert!(history.undo(&c).is_some());
    history.commit(&c, &b, EditorHistoryEffect::Undo, 2);
    assert!(
        history.undo(&b).is_none(),
        "the oldest 900KiB step must be evicted when the redo snapshot grows the combined history past 2MiB"
    );
}
