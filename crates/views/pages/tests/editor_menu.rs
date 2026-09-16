use pages_view::editor::{self, Doc, EditorCursor, EditorDecision, EditorHistoryEffect, History};
use pages_view::editor_menu as menu;

fn doc(text: &str, line: usize, column: usize) -> Doc {
    Doc::new(text, EditorCursor::at(line, column))
}
fn apply(before: &Doc, decision: EditorDecision) -> Doc {
    let EditorDecision::Apply {
        patches,
        cursor,
        history,
    } = decision
    else {
        panic!("menu edit must propose an atomic Apply");
    };
    assert_eq!(history, EditorHistoryEffect::NewGroup);
    editor::apply(before, &patches, cursor)
}

#[test]
fn palette_rewrites_unicode_as_one_undo_step() {
    let before = doc("Title\n- 한글", 1, 8);
    let after = apply(&before, menu::turn(&before, 1, "code"));
    assert_eq!(after, doc("Title\n```\n한글\n```", 2, 6));
    let mut history = History::default();
    history.commit(&before, &after, EditorHistoryEffect::NewGroup, 1);
    let EditorDecision::Apply {
        patches, cursor, ..
    } = history.undo(&after).unwrap()
    else {
        panic!("undo")
    };
    assert_eq!(editor::apply(&after, &patches, cursor), before);
}

#[test]
fn todo_keeps_unicode_and_collapses_the_selection() {
    let mut before = doc("Title\n  - [ ] 👍🏽", 1, 16);
    before.cursor.selection = Some(editor::EditorPosition::new(1, 8));
    let after = apply(&before, menu::toggle_todo(&before, 1));
    assert_eq!(after, doc("Title\n  - [x] 👍🏽", 1, 6));
}

#[test]
fn plus_and_empty_palette_pick_leave_the_fence_intact() {
    let before = doc("Title\n```\ncode\n```", 2, 0);
    let after = apply(&before, menu::insert_below(&before, 2));
    assert_eq!(after, doc("Title\n```\ncode\n```\n", 4, 0));
    let picked = apply(&after, menu::turn_from_slash(&after, 4, 0, false, "h1"));
    assert_eq!(picked, doc("Title\n```\ncode\n```\n# ", 4, 2));
}

#[test]
fn block_operations_keep_fences_whole_and_title_immovable() {
    let before = doc("Title\n```\ncode\n```\npara", 2, 0);
    assert_eq!(
        apply(&before, menu::duplicate(&before, 2)),
        doc("Title\n```\ncode\n```\n```\ncode\n```\npara", 4, 0)
    );
    assert_eq!(
        apply(&before, menu::delete(&before, 2)),
        doc("Title\npara", 1, 0)
    );
    assert_eq!(
        apply(&before, menu::move_block(&before, 2, 1)),
        doc("Title\npara\n```\ncode\n```", 2, 0)
    );
    assert_eq!(menu::move_block(&before, 1, -1), EditorDecision::Noop);
    assert_eq!(menu::delete(&before, 0), EditorDecision::Noop);
}

#[test]
fn drops_use_declared_block_boundaries() {
    let before = doc("Title\n```\ncode\n```\npara", 2, 0);
    assert_eq!(menu::drop_boundaries(&before), vec![1, 4, 5]);
    assert_eq!(
        apply(&before, menu::drop_move(&before, 2, 5)),
        doc("Title\npara\n```\ncode\n```", 2, 0)
    );
    for invalid_or_own in [0, 1, 2, 4, 6] {
        assert_eq!(
            menu::drop_move(&before, 2, invalid_or_own),
            EditorDecision::Noop
        );
    }
}

#[test]
fn slash_filter_is_removed_at_utf8_byte_columns() {
    let before = doc("Title\n한글 /h", 1, 9);
    assert_eq!(
        apply(&before, menu::turn_from_slash(&before, 1, 7, true, "h2")),
        doc("Title\n## 한글 ", 1, 10)
    );
    assert_eq!(
        menu::turn_from_slash(&before, 1, 1, true, "h2"),
        EditorDecision::Noop
    );
}

#[test]
fn menu_lifecycle_filters_at_byte_columns_and_closes_on_caret_movement() {
    let slash = doc("Title\n한글 /", 1, 8);
    let mut state = menu::Menu::default();
    state.after_edit(&slash, Some('/'));
    assert_eq!(state.current(&slash).unwrap().items.len(), 15);
    let filter = doc("Title\n한글 /h", 1, 9);
    state.after_edit(&filter, None);
    let view = state.current(&filter).unwrap();
    assert_eq!(tags(&view), vec!["h1", "h2", "h3"]);
    state.select(&filter, usize::MAX);
    assert_eq!(state.current(&filter).unwrap().selected, 2);
    state.close();
    assert!(state.current(&filter).is_none());
    state.after_edit(&slash, Some('/'));
    state.after_edit(&doc("Title\n한글 /hz", 1, 10), None);
    assert!(state.current(&filter).is_none());
}

/// A picture is picked from the palette like a block turn, but it is neither
/// a turn nor a trigger: the words that asked for it go, the line is left
/// empty for the picture, and the pick tells the view which line waits for
/// the file the host is about to offer.
#[test]
fn a_picture_pick_clears_its_line_and_asks_the_view_for_a_file() {
    let slash = doc("Title\nnotes /image", 1, 12);
    let mut state = menu::Menu::default();
    state.after_edit(&doc("Title\nnotes /", 1, 7), Some('/'));
    state.after_edit(&slash, None);
    assert_eq!(tags(&state.current(&slash).unwrap()), vec![menu::PICTURE]);
    assert_eq!(
        state.intent(&slash, menu::PICTURE).picture_line,
        Some(1),
        "the view opens the picker for this line"
    );
    let (decision, closed) = state.pick(&slash, menu::PICTURE);
    assert_eq!(apply(&slash, decision), doc("Title\nnotes ", 1, 6));
    assert!(
        closed.current(&slash).is_none(),
        "the palette closes behind the pick"
    );
}

fn tags(view: &menu::MenuView) -> Vec<&str> {
    view.items.iter().map(|item| item.0.as_str()).collect()
}

fn names() -> Vec<String> {
    ["alice", "bob", "Carol"].map(String::from).to_vec()
}

#[test]
fn an_at_sign_at_a_word_start_completes_a_member_name() {
    let mut state = menu::Menu::default().with_names(&names());
    let at = doc("Title\nping @", 1, 6);
    state.after_edit(&at, Some('@'));
    assert_eq!(
        tags(&state.current(&at).unwrap()),
        vec!["alice", "bob", "Carol"]
    );
    let filtered = doc("Title\nping @AR", 1, 8);
    state.after_edit(&filtered, None);
    let view = state.current(&filtered).unwrap();
    assert_eq!(tags(&view), vec!["Carol"]);
    assert_eq!(view.items[0].1, "@Carol");
    let (decision, closed) = state.pick(&filtered, "Carol");
    assert_eq!(
        apply(&filtered, decision),
        doc("Title\nping @Carol ", 1, 12)
    );
    assert!(!closed.is_open());
    // an email address is prose, and a space ends the handle
    let mut email = menu::Menu::default().with_names(&names());
    email.after_edit(&doc("Title\nme@", 1, 3), Some('@'));
    assert!(!email.is_open());
    state.after_edit(&doc("Title\nping @AR x", 1, 10), None);
    assert!(!state.is_open());
}

#[test]
fn a_colon_completes_an_emoji_short_code_once_a_letter_narrows_it() {
    let mut state = menu::Menu::default();
    let colon = doc("Title\nship it :", 1, 9);
    state.after_edit(&colon, Some(':'));
    assert!(state.is_open());
    assert!(
        state.current(&colon).is_none(),
        "nothing to show until a letter"
    );
    let filtered = doc("Title\nship it :roc", 1, 12);
    state.after_edit(&filtered, None);
    let view = state.current(&filtered).unwrap();
    assert_eq!(tags(&view), vec!["rocket"]);
    let (decision, closed) = state.pick(&filtered, "rocket");
    assert_eq!(apply(&filtered, decision), doc("Title\nship it 🚀", 1, 12));
    assert!(!closed.is_open());
    let mut clock = menu::Menu::default();
    clock.after_edit(&doc("Title\nat 12:", 1, 6), Some(':'));
    assert!(!clock.is_open(), "12:30 is a time, not a picker");
}

#[test]
fn the_slash_palette_hands_off_to_the_mention_and_emoji_pickers() {
    let mut state = menu::Menu::default().with_names(&names());
    let slash = doc("Title\n/men", 1, 4);
    state.after_edit(&doc("Title\n/", 1, 1), Some('/'));
    state.after_edit(&slash, None);
    assert_eq!(tags(&state.current(&slash).unwrap()), vec!["mention"]);
    let (decision, next) = state.pick(&slash, "mention");
    let at = apply(&slash, decision);
    assert_eq!(at, doc("Title\n@", 1, 1));
    assert_eq!(
        tags(&next.current(&at).unwrap()),
        vec!["alice", "bob", "Carol"]
    );
}

#[test]
fn cmd_slash_opens_the_format_menu_at_the_caret_and_its_picks_wrap_the_selection() {
    let mut selection = doc("Title\nsome words", 1, 10);
    selection.cursor.selection = Some(editor::EditorPosition::new(1, 5));
    let mut state = menu::Menu::default();
    state.format(&selection);
    let view = state.current(&selection).unwrap();
    assert_eq!(view.line, None, "the format menu floats at the caret");
    assert_eq!(
        tags(&view),
        vec![
            "bold",
            "italic",
            "strike",
            "underline",
            "code",
            "highlight",
            "color",
            "link",
            "comment",
            "ai",
            "align",
            "turn",
            "clear"
        ]
    );
    let (decision, palette) = state.pick(&selection, "color");
    assert_eq!(decision, EditorDecision::Noop);
    let colors = palette.current(&selection).unwrap();
    assert_eq!(
        colors.line, None,
        "the palette floats where the toolbar did"
    );
    assert_eq!(tags(&colors)[..3], ["default", "gray", "brown"]);
    let (decision, closed) = palette.pick(&selection, "red");
    let red = apply(&selection, decision);
    assert_eq!(
        red.text,
        "Title\nsome <span style=\"color:#d44c47\">words</span>"
    );
    assert!(!closed.is_open());
    let (decision, _) = palette.pick(&red, "default");
    assert_eq!(apply(&red, decision).text, "Title\nsome words");
    let (decision, closed) = state.pick(&selection, "highlight");
    assert_eq!(apply(&selection, decision).text, "Title\nsome ==words==");
    assert!(!closed.is_open());
    let (decision, turning) = state.pick(&selection, "turn");
    assert_eq!(decision, EditorDecision::Noop);
    assert_eq!(turning.current(&selection).unwrap().items.len(), 12);
    let intent = state.intent(&selection, "comment");
    assert_eq!(intent.comment_line, Some(1));
    assert_eq!(intent.anchor, Some((5, 10)));
    let mut title = menu::Menu::default();
    title.format(&doc("Title\nbody", 0, 2));
    assert!(!title.is_open());
}

#[test]
fn a_caret_move_floats_the_format_menu_over_a_selection_and_closes_it_over_a_caret() {
    let mut selection = doc("Title\nsome words", 1, 10);
    selection.cursor.selection = Some(editor::EditorPosition::new(1, 5));
    let mut state = menu::Menu::default();
    state.moved(&selection);
    assert_eq!(tags(&state.current(&selection).unwrap())[0], "bold");
    let mut collapsed = selection.clone();
    collapsed.cursor.selection = Some(collapsed.cursor.position);
    state.moved(&collapsed);
    assert!(!state.is_open(), "an empty selection is a caret");
    let mut title = doc("Title\nbody", 0, 3);
    title.cursor.selection = Some(editor::EditorPosition::new(0, 0));
    state.moved(&title);
    assert!(!state.is_open(), "the title takes no marks");
}

#[test]
fn a_pressed_link_opens_its_popover_and_the_picks_open_copy_or_unlink() {
    let document = doc("Title\nsee [docs](https://x.y) now", 1, 0);
    let mut state = menu::Menu::default();
    state.link(&document, 1, 6);
    let view = state.current(&document).unwrap();
    assert_eq!(view.line, Some(1));
    assert_eq!(tags(&view), vec!["open", "copy", "unlink"]);
    assert_eq!(state.intent(&document, "open").link, "https://x.y");
    assert_eq!(state.intent(&document, "copy").copy, "https://x.y");
    let (decision, closed) = state.pick(&document, "unlink");
    assert_eq!(apply(&document, decision).text, "Title\nsee docs now");
    assert!(!closed.is_open());
    let mut prose = menu::Menu::default();
    prose.link(&document, 1, 1);
    assert!(!prose.is_open(), "no popover where no link is");
}

/// A right press on another row opens that row's block menu and, in the same
/// gesture, lands the caret on it: the menu hung on that line stays through
/// the caret move; a caret landing anywhere else folds it.
#[test]
fn a_caret_landing_on_the_block_menus_own_line_keeps_it_open() {
    let mut state = menu::Menu::default();
    let document = doc("Title\nfirst\nsecond", 1, 0);
    state.block(&document, 2);
    assert!(state.is_open());
    state.moved(&doc("Title\nfirst\nsecond", 2, 0));
    assert!(state.is_open(), "the caret landed on the menu's own line");
    state.moved(&doc("Title\nfirst\nsecond", 1, 0));
    assert!(!state.is_open(), "the caret left the menu's line");
}

#[test]
fn the_block_menu_copies_the_block_and_resets_its_formatting() {
    let document = doc("Title\n## a **bold** heading\n```\ncode\n```", 1, 0);
    let mut state = menu::Menu::default();
    state.block(&document, 1);
    assert_eq!(
        tags(&state.current(&document).unwrap()),
        vec![
            "turn",
            "align",
            "comment",
            "ai",
            "copy",
            "clear",
            "duplicate",
            "move-up",
            "move-down",
            "delete"
        ]
    );
    assert_eq!(
        state.intent(&document, "copy").copy,
        "## a **bold** heading"
    );
    let (decision, _) = state.pick(&document, "clear");
    assert_eq!(
        apply(&document, decision).text,
        "Title\na bold heading\n```\ncode\n```"
    );
    state.block(&document, 2);
    assert_eq!(state.intent(&document, "copy").copy, "```\ncode\n```");
    assert!(!tags(&state.current(&document).unwrap()).contains(&"clear"));
}

#[test]
fn menu_lifecycle_plans_do_not_change_the_current_menu_before_commit() {
    let before = doc("Title\n```\ncode\n```", 2, 0);
    let state = menu::Menu::default();
    let (decision, opened) = state.plus(&before, 2);
    assert!(state.current(&before).is_none());
    let after = apply(&before, decision);
    assert_eq!(opened.current(&after).unwrap().items.len(), 15);
    let (decision, closed) = opened.pick(&after, "h1");
    assert!(
        opened.current(&after).is_some(),
        "a rejected pick must leave its old menu available"
    );
    let picked = apply(&after, decision);
    assert_eq!(picked.text, "Title\n```\ncode\n```\n# ");
    assert!(closed.current(&picked).is_none());
}

#[test]
fn menu_lifecycle_rejects_hidden_picks_and_turns_only_offered_blocks() {
    let before = doc("Title\n```\ncode\n```\npara", 4, 0);
    let mut state = menu::Menu::default();
    state.block(&before, 1);
    assert!(
        !state
            .current(&before)
            .unwrap()
            .items
            .iter()
            .any(|item| item.0 == "turn")
    );
    let (decision, unchanged) = state.pick(&before, "turn");
    assert_eq!(decision, EditorDecision::Noop);
    assert_eq!(unchanged, state);
    state.block(&before, 4);
    let (decision, turning) = state.pick(&before, "turn");
    assert_eq!(decision, EditorDecision::Noop);
    assert_eq!(turning.current(&before).unwrap().items.len(), 12);
    let (decision, closed) = turning.pick(&before, "h2");
    assert_eq!(
        apply(&before, decision).text,
        "Title\n```\ncode\n```\n## para"
    );
    assert!(closed.current(&before).is_none());
}

#[test]
fn a_block_offers_comment_without_editing_the_document() {
    let document = doc("Title\nA paragraph", 1, 0);
    let mut menu = menu::Menu::default();
    menu.block(&document, 1);
    let view = menu.current(&document).unwrap();
    assert_eq!(view.line, Some(1));
    assert!(view.items.contains(&("comment".into(), "Comment".into())));
    assert_eq!(menu.intent(&document, "comment").comment_line, Some(1));
    let (decision, closed) = menu.pick(&document, "comment");
    assert!(matches!(decision, EditorDecision::Noop));
    assert!(!closed.is_open());
}

#[test]
fn the_align_submenu_sets_a_side_from_the_toolbar_and_the_block_menu() {
    let mut selection = doc("Title\nsome words", 1, 10);
    selection.cursor.selection = Some(editor::EditorPosition::new(1, 5));
    let mut state = menu::Menu::default();
    state.format(&selection);
    let (decision, sides) = state.pick(&selection, "align");
    assert_eq!(decision, EditorDecision::Noop);
    let view = sides.current(&selection).unwrap();
    assert_eq!(view.line, None, "the submenu floats where the toolbar did");
    assert_eq!(tags(&view), vec!["left", "center", "right"]);
    let (decision, closed) = sides.pick(&selection, "center");
    let centered = apply(&selection, decision);
    assert_eq!(centered.text, "Title\n-> some words");
    assert_eq!(centered.cursor.position, editor::EditorPosition::new(1, 13));
    assert_eq!(
        centered.cursor.selection,
        Some(editor::EditorPosition::new(1, 8))
    );
    assert!(!closed.is_open());

    let block = doc("Title\n# Head\nbody", 1, 0);
    let mut state = menu::Menu::default();
    state.block(&block, 1);
    let (_, sides) = state.pick(&block, "align");
    assert_eq!(
        sides.current(&block).unwrap().line,
        Some(1),
        "the block menu's submenu stays on its block"
    );
    let (decision, _) = sides.pick(&block, "right");
    assert_eq!(apply(&block, decision).text, "Title\n# ->> Head\nbody");
    let (decision, _) = sides.pick(&block, "left");
    assert_eq!(
        decision,
        EditorDecision::Noop,
        "a line already at the start is not rewritten"
    );
}

#[test]
fn ask_ai_lists_the_agents_and_addresses_the_comment_to_the_one_picked() {
    let mut selection = doc("Title\nsome words", 1, 10);
    selection.cursor.selection = Some(editor::EditorPosition::new(1, 5));
    let agents = vec![("Builder".to_owned(), 7), ("Reviewer".to_owned(), 9)];
    let mut state = menu::Menu::default().with_agents(&agents);
    state.format(&selection);
    let (decision, picker) = state.pick(&selection, "ai");
    assert_eq!(decision, EditorDecision::Noop);
    let view = picker.current(&selection).unwrap();
    assert_eq!(tags(&view), vec!["7", "9"]);
    assert_eq!(view.items[0].1, "Builder");
    let intent = picker.intent(&selection, "7");
    assert_eq!(intent.comment_line, Some(1));
    assert_eq!(
        intent.anchor,
        Some((5, 10)),
        "the toolbar's ask pins to the selection"
    );
    assert_eq!(intent.mention, 7);
    let (decision, closed) = picker.pick(&selection, "7");
    assert_eq!(decision, EditorDecision::Noop, "asking edits nothing");
    assert!(!closed.is_open());
    assert_eq!(
        picker.intent(&selection, "8").mention,
        0,
        "an agent not offered is not addressed"
    );

    let block = doc("Title\nA paragraph", 1, 0);
    let mut state = menu::Menu::default().with_agents(&agents);
    state.block(&block, 1);
    let (_, picker) = state.pick(&block, "ai");
    assert_eq!(picker.current(&block).unwrap().line, Some(1));
    let intent = picker.intent(&block, "9");
    assert_eq!(
        (intent.comment_line, intent.anchor, intent.mention),
        (Some(1), None, 9)
    );

    // with nobody to ask the picker says so, and that row asks nobody
    let mut nobody = menu::Menu::default();
    nobody.format(&selection);
    let (_, picker) = nobody.pick(&selection, "ai");
    let view = picker.current(&selection).expect("the picker opens on its one row");
    assert_eq!(tags(&view), vec!["none"]);
    assert_eq!(view.items[0].1, "No active agents");
    let intent = picker.intent(&selection, "none");
    assert_eq!((intent.comment_line, intent.mention), (None, 0));
    let (decision, closed) = picker.pick(&selection, "none");
    assert_eq!(decision, EditorDecision::Noop);
    assert!(!closed.is_open());
}
