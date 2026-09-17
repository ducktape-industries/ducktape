//! Test drive of the basics: typing, splitting, joining, navigation, lists,
//! markdown rules, marks and the slash menu — all through the real UI.

use gpui_kit::component::{Root, Theme, ThemeMode};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, px, size};
use gpui_notion::editor::block::{BlockContent, BlockId};
use gpui_notion::editor::{self, CellPosition, EditorTheme, MarkKind, NotionEditor, types};

struct Harness {
    editor: Entity<NotionEditor>,
    window: AnyWindowHandle,
}

fn setup(cx: &mut TestAppContext) -> Harness {
    cx.update(gpui_kit::init);
    cx.update(editor::init);

    let mut editor = None;
    let handle = cx.open_window(size(px(900.), px(700.)), |window, cx| {
        let view = cx.new(|cx| NotionEditor::new(window, cx));
        editor = Some(view.clone());
        Root::new(view, window, cx)
    });

    let harness = Harness {
        editor: editor.unwrap(),
        window: handle.into(),
    };
    harness.focus_first(cx);
    harness
}

impl Harness {
    /// Run `f` inside the window, with a frame rendered first.
    fn ui<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> R,
    ) -> R {
        let result = cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                let result = f(window, cx);
                // A frame after the interaction delivers focus changes.
                window.render_frame(cx);
                result
            })
            .unwrap();
        // Deferred work — focus moves, overlays — lands before the next step.
        cx.run_until_parked();
        result
    }

    fn focus_first(&self, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| {
            window.click(("block", 1usize), cx);
        });
    }

    /// Type the way a person does: one character at a time, so input rules
    /// and the slash menu see each keystroke.
    fn type_text(&self, text: &str, cx: &mut TestAppContext) {
        for ch in text.chars() {
            let ch = ch.to_string();
            self.ui(cx, |window, cx| window.input(&ch, cx));
        }
    }

    fn press(&self, key: &str, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| window.press(key, cx));
    }

    fn texts(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            self.editor
                .read(cx)
                .content()
                .into_iter()
                .map(|block| block.text)
                .collect()
        })
    }

    fn types(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            self.editor
                .read(cx)
                .content()
                .into_iter()
                .map(|block| block.ty.to_string())
                .collect()
        })
    }

    fn indents(&self, cx: &mut TestAppContext) -> Vec<usize> {
        cx.update(|cx| {
            self.editor
                .read(cx)
                .content()
                .into_iter()
                .map(|block| block.indent)
                .collect()
        })
    }

    /// (block index, caret offset) of the focused block.
    fn caret(&self, cx: &mut TestAppContext) -> Option<(usize, usize)> {
        cx.update(|cx| {
            let editor = self.editor.read(cx);
            let id = editor.focused_id()?;
            let ix = editor.index_of(id)?;
            let offset = editor.caret_offset(id, cx)?;
            Some((ix, offset))
        })
    }
}

#[gpui_kit::test]
fn types_text_into_the_first_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("Hello editor", cx);
    assert_eq!(harness.texts(cx), vec!["Hello editor"]);
}

#[gpui_kit::test]
fn enter_splits_a_block_at_the_caret(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("Hello world", cx);
    harness.press("left", cx);
    harness.press("left", cx);
    harness.press("left", cx);
    harness.press("left", cx);
    harness.press("left", cx);
    harness.press("enter", cx);

    assert_eq!(harness.texts(cx), vec!["Hello ", "world"]);
    assert_eq!(harness.caret(cx), Some((1, 0)));
}

#[gpui_kit::test]
fn backspace_at_the_start_joins_with_the_previous_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("first", cx);
    harness.press("enter", cx);
    harness.type_text("second", cx);
    assert_eq!(harness.texts(cx), vec!["first", "second"]);

    for _ in 0..6 {
        harness.press("left", cx);
    }
    harness.press("backspace", cx);

    assert_eq!(harness.texts(cx), vec!["firstsecond"]);
    assert_eq!(harness.caret(cx), Some((0, 5)));
}

#[gpui_kit::test]
fn arrows_walk_between_blocks(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    harness.press("up", cx);
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(0));
    harness.press("down", cx);
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(1));

    // Left at offset 0 crosses to the end of the block above.
    harness.press("home", cx);
    harness.press("left", cx);
    assert_eq!(harness.caret(cx), Some((0, 3)));
    harness.press("right", cx);
    assert_eq!(harness.caret(cx), Some((1, 0)));
}

#[gpui_kit::test]
fn markdown_rules_create_nodes(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("# Title", cx);
    assert_eq!(harness.types(cx), vec![types::HEADING]);
    assert_eq!(harness.texts(cx), vec!["Title"]);

    harness.press("enter", cx);
    harness.type_text("- item", cx);
    assert_eq!(
        harness.types(cx),
        vec![types::HEADING, types::BULLET_LIST]
    );
    assert_eq!(harness.texts(cx)[1], "item");

    harness.press("enter", cx);
    harness.type_text("1. one", cx);
    assert_eq!(harness.types(cx)[2], types::ORDERED_LIST);

    harness.press("enter", cx);
    harness.press("enter", cx);
    harness.type_text("> quoted", cx);
    assert_eq!(*harness.types(cx).last().unwrap(), types::BLOCKQUOTE);
}

#[gpui_kit::test]
fn enter_continues_a_list_and_an_empty_item_leaves_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("- one", cx);
    harness.press("enter", cx);
    assert_eq!(harness.types(cx)[1], types::BULLET_LIST);

    harness.type_text("two", cx);
    harness.press("enter", cx);
    harness.press("enter", cx);
    assert_eq!(*harness.types(cx).last().unwrap(), types::PARAGRAPH);
}

#[gpui_kit::test]
fn tab_nests_a_list_item_and_shift_tab_lifts_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("- one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    harness.press("tab", cx);
    assert_eq!(harness.indents(cx), vec![0, 1]);
    harness.press("shift-tab", cx);
    assert_eq!(harness.indents(cx), vec![0, 0]);
    // The first item has nothing to nest under.
    harness.press("up", cx);
    harness.press("tab", cx);
    assert_eq!(harness.indents(cx), vec![0, 0]);
}

#[gpui_kit::test]
fn inline_rules_apply_marks(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("say **bold** now", cx);

    assert_eq!(harness.texts(cx), vec!["say bold now"]);
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let block = &editor.content()[0];
        assert!(block.marks.has(&gpui_notion::editor::MarkKind::Bold, &(4..8)));
        assert!(!block.marks.has(&gpui_notion::editor::MarkKind::Bold, &(9..12)));
    });
}

#[gpui_kit::test]
fn the_bold_shortcut_marks_the_selection(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("hello", cx);
    harness.press("shift-home", cx);
    harness.press("secondary-b", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(editor.content()[0]
            .marks
            .has(&gpui_notion::editor::MarkKind::Bold, &(0..5)));
    });
}

#[gpui_kit::test]
fn the_slash_menu_filters_and_runs_an_item(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).slash_menu_is_open()));

    harness.type_text("head", cx);
    let titles = cx.update(|cx| {
        harness
            .editor
            .read(cx)
            .slash_items(cx)
            .into_iter()
            .map(|item| item.title().to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        titles,
        vec![
            "Heading 1".to_string(),
            "Heading 2".to_string(),
            "Heading 3".to_string()
        ]
    );

    harness.press("down", cx);
    harness.press("enter", cx);

    assert_eq!(harness.types(cx), vec![types::HEADING]);
    assert_eq!(harness.texts(cx), vec![""]);
    assert!(!cx.update(|cx| harness.editor.read(cx).slash_menu_is_open()));
    cx.update(|cx| assert_eq!(harness.editor.read(cx).content()[0].attrs.level, 2));
}

#[gpui_kit::test]
fn escape_closes_the_slash_menu_and_keeps_the_text(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("a /he", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).slash_menu_is_open()));
    harness.press("escape", cx);
    assert!(!cx.update(|cx| harness.editor.read(cx).slash_menu_is_open()));
    assert_eq!(harness.texts(cx), vec!["a /he"]);
}

#[gpui_kit::test]
fn typing_after_bold_text_continues_the_mark(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("hi", cx);
    harness.press("shift-home", cx);
    harness.press("secondary-b", cx);
    harness.press("end", cx);
    harness.type_text("!", cx);

    cx.update(|cx| {
        let block = &harness.editor.read(cx).content()[0];
        assert!(block.marks.has(&gpui_notion::editor::MarkKind::Bold, &(0..3)));
    });
}

#[gpui_kit::test]
fn typing_after_an_inline_rule_is_plain(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("*it* rest", cx);
    cx.update(|cx| {
        let block = &harness.editor.read(cx).content()[0];
        assert_eq!(block.text, "it rest");
        assert!(block.marks.has(&gpui_notion::editor::MarkKind::Italic, &(0..2)));
        assert!(!block.marks.has(&gpui_notion::editor::MarkKind::Italic, &(3..7)));
    });
}

#[gpui_kit::test]
fn the_gutter_plus_adds_a_block_below(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("first", cx);
    let id = cx.update(|cx| harness.editor.read(cx).content().len());
    assert_eq!(id, 1);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);
        window.click(("insert", 1usize), cx);
    })
    .unwrap();

    assert_eq!(harness.texts(cx), vec!["first", "/"]);
}

#[gpui_kit::test]
fn a_block_can_be_dragged_below_another(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    assert_eq!(harness.texts(cx), vec!["one", "two"]);

    cx.update(|cx| {
        harness.editor.clone().update(cx, |editor, cx| {
            editor.reorder_block(0, 2, cx);
        })
    });
    assert_eq!(harness.texts(cx), vec!["two", "one"]);
}

#[gpui_kit::test]
fn a_document_loads_exactly_the_blocks_it_was_given(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(editor::init);

    let content = vec![
        gpui_notion::editor::block::BlockContent::paragraph("one"),
        gpui_notion::editor::block::BlockContent::paragraph("two"),
        gpui_notion::editor::block::BlockContent::paragraph(""),
    ];
    let mut view = None;
    let handle = cx.open_window(size(px(900.), px(700.)), |window, cx| {
        let editor = cx.new(|cx| NotionEditor::with_content(content.clone(), window, cx));
        view = Some(editor.clone());
        Root::new(editor, window, cx)
    });
    let view = view.unwrap();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    cx.update(|cx| {
        let editor = view.read(cx);
        assert_eq!(
            editor
                .content()
                .into_iter()
                .map(|block| block.text)
                .collect::<Vec<_>>(),
            vec!["one", "two", ""]
        );
        assert!(!editor.slash_menu_is_open());
    });
}

#[gpui_kit::test]
fn undo_reverts_typing_and_redo_restores_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("hello", cx);
    assert_eq!(harness.texts(cx), vec!["hello"]);

    harness.press("secondary-z", cx);
    assert_eq!(harness.texts(cx), vec![""]);

    assert!(cx.update(|cx| harness.editor.read(cx).focused_id().is_some()), "focus after undo");
    harness.press("secondary-y", cx);
    assert_eq!(harness.texts(cx), vec!["hello"]);
}

#[gpui_kit::test]
fn undo_reverts_a_split(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one two", cx);
    harness.press("enter", cx);
    assert_eq!(harness.texts(cx), vec!["one two", ""]);

    harness.press("secondary-z", cx);
    assert_eq!(harness.texts(cx), vec!["one two"]);
}

#[gpui_kit::test]
fn undo_reverts_a_node_change(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("title", cx);
    harness.press("secondary-alt-1", cx);
    assert_eq!(harness.types(cx), vec![types::HEADING]);

    harness.press("secondary-z", cx);
    assert_eq!(harness.types(cx), vec![types::PARAGRAPH]);
    assert_eq!(harness.texts(cx), vec!["title"]);
}

#[gpui_kit::test]
fn redo_through_the_api(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("hello", cx);
    cx.update_window(harness.window, |_, window, cx| {
        harness.editor.clone().update(cx, |editor, cx| editor.undo(window, cx));
    }).unwrap();
    assert_eq!(harness.texts(cx), vec![""]);
    cx.update_window(harness.window, |_, window, cx| {
        harness.editor.clone().update(cx, |editor, cx| editor.redo(window, cx));
    }).unwrap();
    assert_eq!(harness.texts(cx), vec!["hello"]);
}

#[gpui_kit::test]
fn clicking_a_block_focuses_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let (focused, handle_focused) = cx
        .update_window(harness.window, |_, window, cx| {
            let editor = harness.editor.read(cx);
            let id = editor.focused_id();
            let block = &editor.content();
            let _ = block;
            let handle = harness
                .editor
                .read(cx)
                .block_focus_handle(0, cx)
                .map(|h| h.is_focused(window));
            (id, handle)
        })
        .unwrap();
    assert_eq!(handle_focused, Some(true), "the input has keyboard focus");
    assert!(focused.is_some(), "the editor tracks the clicked block");
}

#[gpui_kit::test]
fn the_toolbar_appears_over_a_selection(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("select me", cx);
    assert!(!cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));

    harness.press("shift-home", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("mark-bold", cx);
    })
    .unwrap();

    cx.update(|cx| {
        assert!(harness.editor.read(cx).content()[0]
            .marks
            .has(&gpui_notion::editor::MarkKind::Bold, &(0..9)));
    });
}

#[gpui_kit::test]
fn the_link_editor_sets_a_link_on_the_selection(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("tiptap", cx);
    harness.press("shift-home", cx);
    harness.press("secondary-shift-k", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).link_editor_is_open()));

    harness.type_text("tiptap.dev", cx);
    harness.press("enter", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(!editor.link_editor_is_open());
        let block = &editor.content()[0];
        let mark = block
            .marks
            .mark_at(
                &gpui_notion::editor::MarkKind::Link(Default::default()),
                0,
            )
            .expect("link mark");
        match &mark.kind {
            gpui_notion::editor::MarkKind::Link(href) => {
                assert_eq!(href.as_ref(), "https://tiptap.dev")
            }
            other => panic!("unexpected mark {other:?}"),
        }
    });
}

#[gpui_kit::test]
fn setting_a_link_tells_the_host_the_document_changed(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let changes: std::rc::Rc<std::cell::Cell<usize>> = Default::default();
    let counter = changes.clone();
    cx.update(|cx| {
        cx.subscribe(
            &harness.editor,
            move |_, _: &gpui_notion::editor::DocumentChanged, _| {
                counter.set(counter.get() + 1)
            },
        )
        .detach()
    });

    harness.type_text("tiptap", cx);
    let typed = changes.get();
    harness.press("shift-home", cx);
    harness.press("secondary-shift-k", cx);
    harness.type_text("tiptap.dev", cx);
    harness.press("enter", cx);

    // Without this the link lives only in the editor: the host keeps the text
    // it was last told about, and the next install drops the mark.
    assert!(changes.get() > typed, "setting a link is a document change");
}

#[gpui_kit::test]
fn pressing_a_link_hands_its_href_to_the_host(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let pressed: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
    let sink = pressed.clone();
    cx.update(|cx| {
        cx.subscribe(
            &harness.editor,
            move |_, event: &gpui_notion::editor::LinkPressed, _| {
                sink.borrow_mut().push(event.0.to_string())
            },
        )
        .detach()
    });

    harness.type_text("tiptap", cx);
    harness.press("shift-home", cx);
    harness.press("secondary-shift-k", cx);
    harness.type_text("tiptap.dev", cx);
    harness.press("enter", cx);
    assert!(pressed.borrow().is_empty(), "writing a link is not pressing it");

    harness.ui(cx, |window, cx| window.click(("block", 1usize), cx));
    assert_eq!(pressed.borrow().as_slice(), ["https://tiptap.dev"]);
}

#[gpui_kit::test]
fn pressing_plain_text_hands_the_host_nothing(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let pressed: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
    let sink = pressed.clone();
    cx.update(|cx| {
        cx.subscribe(
            &harness.editor,
            move |_, event: &gpui_notion::editor::LinkPressed, _| {
                sink.borrow_mut().push(event.0.to_string())
            },
        )
        .detach()
    });

    harness.type_text("tiptap", cx);
    harness.ui(cx, |window, cx| window.click(("block", 1usize), cx));
    assert!(pressed.borrow().is_empty());
}

#[gpui_kit::test]
fn pasted_lines_become_blocks(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.ui(cx, |window, cx| {
        window.input("# Title\n- one\n- two", cx);
    });

    assert_eq!(harness.texts(cx), vec!["Title", "one", "two"]);
    assert_eq!(
        harness.types(cx),
        vec![types::HEADING, types::BULLET_LIST, types::BULLET_LIST]
    );
}

#[gpui_kit::test]
fn shift_down_selects_whole_blocks_and_backspace_removes_them(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("enter", cx);
    harness.type_text("three", cx);
    harness.press("up", cx);

    harness.press("shift-down", cx);
    assert_eq!(
        cx.update(|cx| harness.editor.read(cx).selected_blocks().len()),
        2
    );

    harness.press("backspace", cx);
    assert_eq!(harness.texts(cx), vec!["one"]);
}

#[gpui_kit::test]
fn select_all_escalates_from_block_to_document(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    harness.press("secondary-a", cx);
    assert!(!cx.update(|cx| harness.editor.read(cx).has_block_selection()));

    harness.press("secondary-a", cx);
    assert_eq!(
        cx.update(|cx| harness.editor.read(cx).selected_blocks().len()),
        2
    );
}

#[gpui_kit::test]
fn dragging_the_handle_reorders_blocks(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("enter", cx);
    harness.type_text("three", cx);
    assert_eq!(harness.texts(cx), vec!["one", "two", "three"]);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);
        window.drag_to(("drag", 1usize), ("block", 3usize), cx);
    })
    .unwrap();
    cx.run_until_parked();

    // The pointer ends on the upper half of the third block, so the dragged
    // block lands above it.
    assert_eq!(harness.texts(cx), vec!["two", "one", "three"]);
}

#[gpui_kit::test]
fn shift_enter_breaks_the_line_inside_a_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("shift-enter", cx);
    harness.type_text("two", cx);

    assert_eq!(harness.texts(cx), vec!["one\ntwo"]);
}

#[gpui_kit::test]
fn arrows_move_within_a_wrapped_block_before_leaving_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("first", cx);
    harness.press("enter", cx);
    harness.type_text("one", cx);
    harness.press("shift-enter", cx);
    harness.type_text("two", cx);

    // The caret is on the second row of the second block: up stays inside it.
    harness.press("up", cx);
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(1));
    harness.press("up", cx);
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(0));
}

#[gpui_kit::test]
fn clicking_below_the_document_appends_a_paragraph(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("only", cx);

    harness.ui(cx, |window, cx| window.click("trailing-space", cx));
    assert_eq!(harness.texts(cx), vec!["only", ""]);
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(1));
}

#[gpui_kit::test]
fn deleting_the_last_block_leaves_an_empty_paragraph(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("# gone", cx);
    harness.press("secondary-shift-backspace", cx);

    assert_eq!(harness.texts(cx), vec![""]);
    assert_eq!(harness.types(cx), vec![types::PARAGRAPH]);
}

#[gpui_kit::test]
fn the_emoji_menu_replaces_the_shortcode(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("ship it :rocket", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));

    harness.press("enter", cx);
    assert_eq!(harness.texts(cx), vec!["ship it 🚀"]);
    assert!(!cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));
}

#[gpui_kit::test]
fn the_mention_menu_inserts_a_marked_name(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("ping @ada", cx);
    assert!(cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));

    harness.press("enter", cx);
    assert_eq!(harness.texts(cx), vec!["ping @Ada Lovelace "]);
    cx.update(|cx| {
        let block = &harness.editor.read(cx).content()[0];
        assert!(block.marks.has(
            &gpui_notion::editor::MarkKind::Mention(Default::default()),
            &(5..18)
        ));
    });
}

#[gpui_kit::test]
fn a_colon_in_prose_does_not_hold_a_menu_open(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("note: something", cx);
    assert!(!cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));
    assert_eq!(harness.texts(cx), vec!["note: something"]);
}

#[gpui_kit::test]
fn arrow_down_leaves_a_code_block_at_the_end_of_the_document(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("```", cx);
    harness.type_text(" ", cx);
    assert_eq!(harness.types(cx), vec![types::CODE_BLOCK]);

    harness.type_text("let x = 1;", cx);
    harness.press("down", cx);

    assert_eq!(
        harness.types(cx),
        vec![types::CODE_BLOCK, types::PARAGRAPH]
    );
    assert_eq!(harness.caret(cx).map(|(ix, _)| ix), Some(1));
}

#[gpui_kit::test]
fn enter_inside_a_code_block_adds_a_line(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("``` ", cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    assert_eq!(harness.texts(cx), vec!["one\ntwo"]);
    assert_eq!(harness.types(cx), vec![types::CODE_BLOCK]);
}

#[gpui_kit::test]
fn enter_in_the_middle_of_a_heading_keeps_both_halves_headings(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("# one two", cx);
    for _ in 0..4 {
        harness.press("left", cx);
    }
    harness.press("enter", cx);

    assert_eq!(harness.texts(cx), vec!["one", " two"]);
    assert_eq!(harness.types(cx), vec![types::HEADING, types::HEADING]);

    // At the end of a heading, Enter starts a paragraph instead.
    harness.press("end", cx);
    harness.press("enter", cx);
    assert_eq!(
        harness.types(cx),
        vec![types::HEADING, types::HEADING, types::PARAGRAPH]
    );
}

#[gpui_kit::test]
fn the_slash_menu_groups_items_in_template_order(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/", cx);

    let groups: Vec<&str> = cx.update(|cx| {
        let mut seen: Vec<&str> = Vec::new();
        for item in harness.editor.read(cx).suggestion_items(cx) {
            if seen.last() != Some(&item.group()) {
                seen.push(item.group());
            }
        }
        seen
    });
    assert_eq!(groups, vec!["Style", "Insert", "Upload"]);
}

#[gpui_kit::test]
fn the_emoji_entry_of_the_slash_menu_opens_the_emoji_menu(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/emoji", cx);
    harness.press("enter", cx);

    assert!(cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));
    harness.type_text("rocket", cx);
    harness.press("enter", cx);
    assert_eq!(harness.texts(cx), vec!["🚀"]);
}

#[gpui_kit::test]
fn an_image_block_takes_a_dropped_file(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/image", cx);
    harness.press("enter", cx);
    assert_eq!(harness.types(cx)[0], types::IMAGE);

    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0)).unwrap();
    cx.update(|cx| {
        harness.editor.clone().update(cx, |editor, cx| {
            editor.set_image_source(id, std::path::PathBuf::from("/tmp/picture.png"), cx)
        })
    });

    cx.update(|cx| {
        let block = &harness.editor.read(cx).content()[0];
        assert_eq!(block.attrs.src.as_deref(), Some("/tmp/picture.png"));
        assert_eq!(block.attrs.alt.as_deref(), Some("picture.png"));
    });
}

#[gpui_kit::test]
fn dragging_across_blocks_selects_them(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("enter", cx);
    harness.type_text("three", cx);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.drag_to(("block", 1usize), ("block", 3usize), cx);
    })
    .unwrap();
    cx.run_until_parked();

    assert_eq!(cx.update(|cx| harness.editor.read(cx).selected_blocks().len()), 3);

    // Backspace on that selection takes all three blocks away.
    harness.ui(cx, |window, cx| window.press("backspace", cx));
    assert_eq!(harness.texts(cx), vec![""]);
}

#[gpui_kit::test]
fn shift_clicking_another_block_selects_the_range(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("enter", cx);
    harness.type_text("three", cx);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("block", 1usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let position = window.find(("block", 3usize)).bounds().center();
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                button: gpui_kit::MouseButton::Left,
                position,
                modifiers: gpui_kit::Modifiers {
                    shift: true,
                    ..Default::default()
                },
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    assert_eq!(cx.update(|cx| harness.editor.read(cx).selected_blocks().len()), 3);
}

#[gpui_kit::test]
fn typing_over_selected_blocks_replaces_them(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("shift-up", cx);
    assert_eq!(cx.update(|cx| harness.editor.read(cx).selected_blocks().len()), 2);

    harness.type_text("x", cx);
    assert_eq!(harness.texts(cx), vec!["x"]);
}

#[gpui_kit::test]
fn bold_over_selected_blocks_marks_all_of_them(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("shift-up", cx);

    harness.press("secondary-b", cx);
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for block in editor.content() {
            assert!(
                block.marks.has(&MarkKind::Bold, &(0..block.text.len())),
                "block {:?} is not bold",
                block.text
            );
        }
    });

    // A second press takes it off every block again.
    harness.press("secondary-b", cx);
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for block in editor.content() {
            assert!(!block.marks.has(&MarkKind::Bold, &(0..block.text.len())));
        }
    });
}

#[gpui_kit::test]
fn turning_selected_blocks_into_a_list_changes_all_of_them(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("shift-up", cx);

    harness.press("secondary-shift-8", cx);
    assert_eq!(
        harness.types(cx),
        vec![types::BULLET_LIST, types::BULLET_LIST]
    );
}

#[gpui_kit::test]
fn commenting_a_selection_opens_a_thread(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("needs review", cx);
    harness.ui(cx, |window, cx| {
        window.press("secondary-a", cx);
        window.press("secondary-shift-m", cx);
    });

    let (threads, quote) = cx.update(|cx| {
        let editor = harness.editor.read(cx);
        (
            editor.comment_threads().len(),
            editor
                .comment_threads()
                .first()
                .map(|thread| thread.quote().to_string()),
        )
    });
    assert_eq!(threads, 1);
    assert_eq!(quote.as_deref(), Some("needs review"));

    // The commented range carries the mark that anchors the thread.
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let id = editor.comment_threads()[0].id();
        let block = &editor.content()[0];
        assert!(block.marks.has(&MarkKind::Comment(id), &(0..block.text.len())));
    });

    // Typing into the draft and pressing Enter posts the comment.
    harness.ui(cx, |window, cx| {
        window.input("looks good", cx);
        window.press("enter", cx);
    });
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let thread = &editor.comment_threads()[0];
        assert_eq!(thread.comments().len(), 1);
        assert_eq!(thread.comments()[0].body(), "looks good");
        assert_eq!(thread.comments()[0].author(), "You");
    });
}

#[gpui_kit::test]
fn an_abandoned_comment_leaves_no_thread_behind(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("draft text", cx);
    harness.ui(cx, |window, cx| {
        window.press("secondary-a", cx);
        window.press("secondary-shift-m", cx);
        window.press("escape", cx);
    });

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(editor.comment_threads().is_empty());
        assert!(editor.content()[0].marks.is_empty());
    });
}

#[gpui_kit::test]
fn resolving_a_thread_takes_the_highlight_off(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("ship it", cx);
    harness.ui(cx, |window, cx| {
        window.press("secondary-a", cx);
        window.press("secondary-shift-m", cx);
        window.input("done", cx);
        window.press("enter", cx);
    });

    let id = cx.update(|cx| harness.editor.read(cx).comment_threads()[0].id());
    harness.ui(cx, |window, cx| window.click("resolve-thread", cx));

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let thread = editor.comment_thread(id).expect("thread is kept");
        assert!(thread.is_resolved());
        assert!(editor.content()[0].marks.is_empty());
        assert!(editor.open_thread().is_none());
    });
}

#[gpui_kit::test]
fn the_slash_menu_inserts_a_table_and_tab_walks_its_cells(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);

    let (ty, rows, columns) = cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let id = editor.block_id_at(0).expect("a table block");
        let grid = editor.grid(id).expect("a grid beside it");
        (editor.content()[0].ty.to_string(), grid.rows(), grid.columns())
    });
    assert_eq!(ty, types::TABLE);
    assert_eq!((rows, columns), (3, 3));

    // The caret starts in the header cell; Tab walks across the row.
    harness.type_text("Name", cx);
    harness.press("tab", cx);
    harness.type_text("Role", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let id = editor.block_id_at(0).unwrap();
        let grid = editor.grid(id).unwrap();
        assert_eq!(grid.cell(CellPosition::new(0, 0)).unwrap().text(), "Name");
        assert_eq!(grid.cell(CellPosition::new(0, 1)).unwrap().text(), "Role");
    });
}

#[gpui_kit::test]
fn tab_in_the_last_cell_adds_a_row(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);

    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());
    // Walk to the last cell: 3 x 3 - 1 tabs.
    for _ in 0..8 {
        harness.press("tab", cx);
    }
    harness.press("tab", cx);

    let rows = cx.update(|cx| harness.editor.read(cx).grid(id).unwrap().rows());
    assert_eq!(rows, 4);
}

#[gpui_kit::test]
fn a_table_survives_undo_and_redo_with_its_text(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    harness.type_text("Kept", cx);

    harness.press("secondary-z", cx);
    harness.press("secondary-shift-z", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let id = editor.block_id_at(0).expect("the table is back");
        let grid = editor.grid(id).expect("with a grid");
        assert_eq!(grid.cell(CellPosition::new(0, 0)).unwrap().text(), "Kept");
    });
}

#[gpui_kit::test]
fn a_table_grows_and_shrinks_by_row_and_column(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());

    // Type in the header so the shape is observable after the edits.
    harness.type_text("A", cx);
    harness.press("tab", cx);
    harness.type_text("B", cx);

    // The controls show while the pointer is over the table.
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", id.0 as usize), cx);
        window.render_frame(cx);
        window.click(("add-column", id.0 as usize), cx);
        window.render_frame(cx);
        window.hover(("block", id.0 as usize), cx);
        window.render_frame(cx);
        window.click(("add-row", id.0 as usize), cx);
    })
    .unwrap();
    cx.run_until_parked();

    let (rows, columns) = cx.update(|cx| {
        let grid = harness.editor.read(cx).grid(id).unwrap();
        (grid.rows(), grid.columns())
    });
    assert_eq!((rows, columns), (4, 4));

    // The row control beside the first row takes that row away.
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", id.0 as usize), cx);
        window.render_frame(cx);
        window.click(("drop-row", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();

    let (rows, header) = cx.update(|cx| {
        let grid = harness.editor.read(cx).grid(id).unwrap();
        (
            grid.rows(),
            grid.cell(CellPosition::new(0, 0)).unwrap().text().to_string(),
        )
    });
    assert_eq!(rows, 3);
    assert_eq!(header, "", "the row carrying A was the one removed");
}

#[gpui_kit::test]
fn a_table_copies_as_a_markdown_table(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    harness.type_text("Name", cx);
    harness.press("tab", cx);
    harness.type_text("Role", cx);

    harness.press("escape", cx);
    let markdown = cx.update(|cx| harness.editor.read(cx).selected_markdown(cx));
    assert!(
        markdown.starts_with("| Name | Role |"),
        "unexpected markdown: {markdown}"
    );
    assert!(markdown.contains("| --- | --- |"));
}

#[gpui_kit::test]
fn the_toolbar_waits_for_the_button_to_come_up(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("select me", cx);

    // Mid-drag the toolbar stays away, however much text is covered.
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let bounds = window.find(("block", 1usize)).bounds();
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                button: gpui_kit::MouseButton::Left,
                position: bounds.center(),
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    assert!(cx.update(|cx| harness.editor.read(cx).press_in_progress()));
    assert!(!cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));

    // Once the button comes up a selection shows it again.
    cx.update_window(harness.window, |_, window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                button: gpui_kit::MouseButton::Left,
                position: window.find(("block", 1usize)).bounds().center(),
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
        window.render_frame(cx);
        window.press("secondary-a", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));
}

#[gpui_kit::test]
fn dragging_a_selected_block_moves_the_whole_selection(cx: &mut TestAppContext) {
    let harness = setup(cx);
    for (ix, text) in ["one", "two", "three", "four"].iter().enumerate() {
        if ix > 0 {
            harness.press("enter", cx);
        }
        harness.type_text(text, cx);
    }
    assert_eq!(harness.texts(cx), vec!["one", "two", "three", "four"]);

    // Select "one" and "two", then drag the handle of "one" past "four".
    harness.ui(cx, |window, cx| {
        window.click(("block", 1usize), cx);
    });
    harness.press("shift-down", cx);
    assert_eq!(cx.update(|cx| harness.editor.read(cx).selected_blocks().len()), 2);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);
        window.drag_to(("drag", 1usize), ("block", 4usize), cx);
    })
    .unwrap();
    cx.run_until_parked();

    assert_eq!(harness.texts(cx), vec!["three", "one", "two", "four"]);
}

#[gpui_kit::test]
fn duplicating_a_selection_copies_every_block_below_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("shift-up", cx);

    harness.press("secondary-d", cx);
    assert_eq!(harness.texts(cx), vec!["one", "two", "one", "two"]);
    assert_eq!(cx.update(|cx| harness.editor.read(cx).selected_blocks().len()), 2);
}

#[gpui_kit::test]
fn the_comment_box_takes_the_toolbar_off_screen(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one line", cx);
    harness.press("secondary-a", cx);
    assert!(
        cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)),
        "a selection shows the toolbar"
    );

    harness.press("secondary-shift-m", cx);
    assert!(
        !cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)),
        "the comment box replaces it"
    );

    harness.press("escape", cx);
    assert!(
        cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)),
        "cancelling the comment gives the selection, and the toolbar, back"
    );
}

#[gpui_kit::test]
fn clicking_different_lines_of_a_block_does_not_move_the_page(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text(
        "This paragraph is deliberately long enough that it wraps onto several rows inside its own input, which is where a click on one row must not change the height of anything.",
        cx,
    );
    harness.press("enter", cx);
    harness.type_text("below", cx);

    let heights = |cx: &mut TestAppContext| -> (f32, f32) {
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            (
                f32::from(window.find(("block", 1usize)).bounds().size.height),
                f32::from(window.find(("block", 2usize)).bounds().origin.y),
            )
        })
        .unwrap()
    };

    let first = heights(cx);
    for offset in [4., 20., 40., 60.] {
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            window.click_at(("block", 1usize), gpui_kit::point(px(200.), px(offset)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let now = heights(cx);
        assert_eq!(
            now, first,
            "clicking at y={offset} changed the block height or what follows it"
        );
    }
}

#[gpui_kit::test]
fn clicking_the_last_row_does_not_scroll_the_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text(
        "This paragraph is deliberately long enough that it wraps onto several rows inside its own input, which is where a click on one row must not shift the text that is already on screen.",
        cx,
    );

    let offset_after_click = |y: f32, cx: &mut TestAppContext| -> (f32, f32) {
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            window.click_at(("block", 1usize), gpui_kit::point(px(200.), px(y)), cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let editor = harness.editor.read(cx);
            let id = editor.block_id_at(0).unwrap();
            let offset = editor.block_scroll_offset(id, cx).unwrap();
            (f32::from(offset.x), f32::from(offset.y))
        })
    };

    let top = offset_after_click(4., cx);
    let bottom = offset_after_click(60., cx);
    assert_eq!(top, (0., 0.), "the block starts unscrolled");
    assert_eq!(bottom, top, "clicking the last row scrolled the block");

    // The reason it cannot scroll: the text area is never shorter than the
    // text, whatever the input keeps for itself.
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for block in editor.content() {
            let id = editor.block_id_at(0).unwrap();
            let _ = block;
            assert!(
                editor.text_area_fits_text(id, cx),
                "the text area is shorter than the text in it"
            );
        }
    });
}

#[gpui_kit::test]
fn every_block_gets_a_text_area_that_fits_its_text(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("# A heading that is long enough to wrap once it reaches the end of the column", cx);
    harness.press("enter", cx);
    harness.type_text(
        "A paragraph long enough to wrap across three rows, which is the shape that makes a short text area show up as text jumping whenever the caret changes row.",
        cx,
    );
    harness.press("enter", cx);
    harness.type_text("- a list item", cx);

    cx.update_window(harness.window, |_, window, cx| {
        // A second frame lets the measured inset settle.
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for ix in 0..editor.content().len() {
            let id = editor.block_id_at(ix).unwrap();
            assert!(
                editor.text_area_fits_text(id, cx),
                "block {ix} has a text area shorter than its text"
            );
            assert_eq!(
                editor.block_scroll_offset(id, cx).map(|p| f32::from(p.y)),
                Some(0.),
                "block {ix} scrolled inside itself"
            );
        }
    });
}

#[gpui_kit::test]
fn a_cell_grows_to_hold_text_that_wraps(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());

    let one_row = cx.update(|cx| {
        f32::from(harness.editor.read(cx).grid(id).unwrap().row_text_height(0))
    });

    harness.type_text(
        "a cell with quite a lot of text in it, far more than one row can hold",
        cx,
    );
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();

    let (wrapped, fits, scroll) = cx.update(|cx| {
        let editor = harness.editor.read(cx);
        (
            f32::from(editor.grid(id).unwrap().row_text_height(0)),
            editor.cells_fit_their_text(id, cx),
            editor.cell_scroll_offset(id, CellPosition::new(0, 0), cx),
        )
    });
    assert!(
        wrapped > one_row,
        "the row stayed {one_row} tall for text that has to wrap (now {wrapped})"
    );
    let metrics = cx.update(|cx| {
        harness
            .editor
            .read(cx)
            .cell_metrics(id, CellPosition::new(0, 0), cx)
    });
    assert!(fits, "a cell is shorter than the text in it");
    assert_eq!(
        scroll,
        Some(0.),
        "a cell scrolled inside itself; metrics (needed, area, line, scroll) = {metrics:?}"
    );
}

#[gpui_kit::test]
fn every_cell_of_a_row_is_as_tall_as_the_row(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());

    // One cell of the second row takes two lines; the rest of the row follows.
    cx.update_window(harness.window, |_, window, cx| {
        harness.editor.clone().update(cx, |editor, cx| {
            editor.set_cell_text(
                id,
                CellPosition::new(1, 1),
                "text long enough that this cell has to take a second line for it",
                window,
                cx,
            );
        });
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let grid = editor.grid(id).unwrap();
        assert!(
            grid.row_text_height(1) > grid.row_text_height(0),
            "the row with wrapped text did not grow"
        );
        for column in 0..grid.columns() {
            let at = CellPosition::new(1, column);
            let (needed, area, _, scroll) = editor.cell_metrics(id, at, cx).unwrap();
            assert!(
                area + 0.01 >= needed,
                "cell {column} has {area} of room for {needed} of text"
            );
            assert_eq!(scroll, 0., "cell {column} scrolled inside itself");
        }
        assert!(editor.cells_fit_their_text(id, cx));
    });
}

#[gpui_kit::test]
fn a_table_in_a_loaded_document_comes_back_with_its_cells(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(editor::init);

    let content = vec![
        gpui_notion::editor::block::BlockContent::paragraph("above"),
        editor::table_content(&[&["Name", "Role"], &["Ada", "Author"]]),
    ];
    let mut view = None;
    let handle = cx.open_window(size(px(900.), px(700.)), |window, cx| {
        let editor = cx.new(|cx| NotionEditor::with_content(content.clone(), window, cx));
        view = Some(editor.clone());
        Root::new(editor, window, cx)
    });
    let view = view.unwrap();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    cx.update(|cx| {
        let editor = view.read(cx);
        let id = editor.block_id_at(1).expect("the table block");
        let grid = editor.grid(id).expect("its cells");
        assert_eq!((grid.rows(), grid.columns()), (2, 2));
        assert_eq!(grid.cell(CellPosition::new(0, 0)).unwrap().text(), "Name");
        assert_eq!(grid.cell(CellPosition::new(1, 1)).unwrap().text(), "Author");
        assert_eq!(
            editor.markdown_of(&[id], cx),
            "| Name | Role |\n| --- | --- |\n| Ada | Author |"
        );
    });
}

#[gpui_kit::test]
fn a_table_cell_has_nothing_to_scroll(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    harness.type_text("Name", cx);
    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());

    // Asking a cell to scroll a long way is clamped to whatever room it has
    // below its last line, which for a cell sized to its text is none.
    let states: Vec<_> = cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let grid = editor.grid(id).unwrap();
        grid.positions()
            .map(|at| (at, grid.cell(at).unwrap().state().clone()))
            .collect()
    });
    cx.update_window(harness.window, |_, window, cx| {
        for (_, state) in &states {
            state.update(cx, |state, cx| {
                state.set_scroll_offset(gpui_kit::point(px(0.), px(-500.)), cx)
            });
        }
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for (at, _) in &states {
            assert_eq!(
                editor.cell_scroll_offset(id, *at, cx),
                Some(0.),
                "cell {at:?} had room to scroll"
            );
        }
    });
}

#[gpui_kit::test]
fn a_narrow_window_reflows_instead_of_clipping(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(editor::init);

    let content = vec![gpui_notion::editor::block::BlockContent::paragraph(
        "A paragraph that is comfortably one row wide in a roomy window and has to \
         take several rows in a narrow one, which is the case that used to leave \
         blocks sized for a width they never got.",
    )];
    let mut view = None;
    // Narrower than the 708px content column.
    let handle = cx.open_window(size(px(420.), px(600.)), |window, cx| {
        let editor = cx.new(|cx| NotionEditor::with_content(content.clone(), window, cx));
        view = Some(editor.clone());
        Root::new(editor, window, cx)
    });
    let view = view.unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();

    cx.update(|cx| {
        let editor = view.read(cx);
        let id = editor.block_id_at(0).unwrap();
        assert!(
            editor.text_area_fits_text(id, cx),
            "the block is shorter than its text in a narrow window"
        );
        assert_eq!(
            editor.block_scroll_offset(id, cx).map(|p| f32::from(p.y)),
            Some(0.),
            "the block scrolled inside itself in a narrow window"
        );
    });
}

#[gpui_kit::test]
fn a_drag_that_starts_at_the_left_of_the_text_still_selects_blocks(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    // Press on the first glyph of the block, not in the middle of the row.
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let id = harness.editor.read(cx).block_id_at(0).unwrap();
        let from = harness
            .editor
            .read(cx)
            .block_bounds(id)
            .expect("the block has laid out");
        let to = window.find(("block", 2usize)).bounds();
        window.drag(
            gpui_kit::point(from.left() + px(2.), from.center().y),
            to.center(),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();

    assert_eq!(
        cx.update(|cx| harness.editor.read(cx).selected_blocks().len()),
        2
    );
}

#[gpui_kit::test]
fn the_gutter_controls_appear_only_on_the_hovered_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);

        assert!(
            window.find(("drag", 1usize)).visible(),
            "the hovered block has no handle"
        );
        assert!(
            !window.find(("drag", 2usize)).visible(),
            "a block nobody is pointing at shows its handle"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_toolbar_sits_over_the_selected_text(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("a line to select", cx);
    harness.press("secondary-a", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let id = editor.block_id_at(1).unwrap();
        let text = editor.block_text_origin(id, cx).expect("laid out");
        let anchor = editor.toolbar_anchor_for_test(cx).expect("a toolbar");
        // Both are glyph coordinates, so they line up to the pixel.
        assert!(
            (anchor.x - text.x).abs() < px(2.),
            "the toolbar hangs at {anchor:?}, the text starts at {text:?}"
        );
        assert!(
            anchor.y < text.y,
            "the toolbar is not above the text it belongs to"
        );
    });
}

#[gpui_kit::test]
fn the_toolbar_over_a_block_selection_lines_up_with_its_text(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);
    harness.press("shift-up", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(editor.has_block_selection());
        let id = editor.selected_blocks()[0];
        let text = editor.block_text_origin(id, cx).expect("laid out");
        let anchor = editor.toolbar_anchor_for_test(cx).expect("a toolbar");
        assert!(
            (anchor.x - text.x).abs() < px(2.),
            "the toolbar hangs at {anchor:?}, the block's text starts at {text:?}"
        );
    });
}

#[gpui_kit::test]
fn focus_leaving_the_blocks_takes_the_toolbar_with_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("some words", cx);
    harness.press("enter", cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);

    // Select text in the paragraph, then put the caret in a table cell.
    harness.ui(cx, |window, cx| {
        window.click(("block", 1usize), cx);
        window.press("secondary-a", cx);
    });
    assert!(cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));

    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(1).unwrap());
    cx.update_window(harness.window, |_, window, cx| {
        harness.editor.clone().update(cx, |editor, cx| {
            editor.focus_cell(id, CellPosition::new(0, 0), window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(
            editor.focused_id().is_none(),
            "a block still claims the caret after it moved into a table"
        );
        assert!(
            !editor.selection_toolbar_visible(cx),
            "the toolbar stayed over text the caret has left"
        );
    });
}

#[gpui_kit::test]
fn a_drag_let_go_away_from_the_blocks_clears_the_drop_line(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);
        let from = window.find(("drag", 1usize)).bounds().center();
        let below = window.find("trailing-space").bounds().center();
        window.drag(from, below, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(
            editor.drop_target_for_test().is_none(),
            "the drop line outlived the drag"
        );
    });
    // And the toolbar works again, which a stuck drop target used to prevent.
    harness.ui(cx, |window, cx| {
        window.click(("block", 1usize), cx);
        window.press("secondary-a", cx);
    });
    assert!(cx.update(|cx| harness.editor.read(cx).selection_toolbar_visible(cx)));
}

#[gpui_kit::test]
fn deleting_a_table_takes_its_cells_with_it(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("/table", cx);
    harness.press("enter", cx);
    let id = cx.update(|cx| harness.editor.read(cx).block_id_at(0).unwrap());
    assert!(cx.update(|cx| harness.editor.read(cx).grid(id).is_some()));

    cx.update_window(harness.window, |_, window, cx| {
        harness
            .editor
            .clone()
            .update(cx, |editor, cx| editor.delete_active_block(window, cx));
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert!(editor.grid(id).is_none(), "the grid outlived its block");
        assert!(
            editor.focused_cell().is_none(),
            "a cell of the deleted table still holds the caret"
        );
    });
}


#[gpui_kit::test]
fn typing_to_the_edge_of_the_column_never_leaves_a_block_short(cx: &mut TestAppContext) {
    let harness = setup(cx);
    // Type a word at a time up to and past the wrap point, checking after
    // every word that the block still holds its text without scrolling.
    for word in [
        "typing", "prose", "that", "keeps", "going", "until", "it", "reaches", "the", "right",
        "hand", "edge", "of", "the", "column", "and", "then", "wraps", "onto", "another", "row",
        "and", "keeps", "going", "again",
    ] {
        harness.type_text(word, cx);
        harness.type_text(" ", cx);
        cx.update(|cx| {
            let editor = harness.editor.read(cx);
            let id = editor.block_id_at(0).unwrap();
            assert!(
                editor.text_area_fits_text(id, cx),
                "the block came up short after {word:?}"
            );
            assert_eq!(
                editor.block_scroll_offset(id, cx).map(|p| f32::from(p.y)),
                Some(0.),
                "the block scrolled after {word:?}"
            );
        });
    }
}

#[gpui_kit::test]
fn undo_keeps_a_comment_thread_pointing_at_its_block(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("worth discussing", cx);
    harness.ui(cx, |window, cx| {
        window.press("secondary-a", cx);
        window.press("secondary-shift-m", cx);
        window.input("look at this", cx);
        window.press("enter", cx);
    });
    assert_eq!(
        cx.update(|cx| harness.editor.read(cx).comment_threads().len()),
        1,
        "the comment was not posted"
    );
    harness.press("escape", cx);
    let thread = cx.update(|cx| harness.editor.read(cx).comment_threads()[0].id());

    // A structural change and an undo re-create every block.
    harness.ui(cx, |window, cx| {
        window.click(("block", 1usize), cx);
        window.press("enter", cx);
    });
    harness.press("secondary-z", cx);

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        let kept = editor
            .comment_thread(thread)
            .expect("the thread survived the undo");
        assert_eq!(kept.comments().len(), 1);
        assert_eq!(
            editor.index_of(kept.block()),
            Some(0),
            "the thread points at a block that is no longer in the document"
        );
    });
}

// ---------------------------------------------------------------- theming

/// Open a document with a base font size of `font_size`, the way an
/// application that wants a bigger or smaller editor would set it.
fn setup_zoomed(cx: &mut TestAppContext, font_size: f32) -> (Harness, Vec<BlockId>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        Theme::change(ThemeMode::Light, None, cx);
        cx.global_mut::<Theme>().font_size = px(font_size);
    });
    cx.update(editor::init);

    let content = vec![
        BlockContent::new(editor::types::HEADING, "Title")
            .with_attrs(gpui_notion::editor::BlockAttrs::level(1)),
        BlockContent::paragraph("A paragraph of prose that is long enough to wrap in the column."),
        BlockContent::new(editor::types::BULLET_LIST, "An item"),
    ];
    let mut view = None;
    let handle = cx.open_window(size(px(900.), px(700.)), |window, cx| {
        let editor = cx.new(|cx| NotionEditor::with_content(content.clone(), window, cx));
        view = Some(editor.clone());
        Root::new(editor, window, cx)
    });
    let harness = Harness {
        editor: view.unwrap(),
        window: handle.into(),
    };
    harness.ui(cx, |window, cx| window.render_frame(cx));
    harness.ui(cx, |window, cx| window.render_frame(cx));
    let ids = cx.update(|cx| {
        let editor = harness.editor.read(cx);
        (0..editor.block_count())
            .filter_map(|ix| editor.block_id_at(ix))
            .collect()
    });
    (harness, ids)
}

#[gpui_kit::test]
fn the_base_font_size_scales_the_whole_document(cx: &mut TestAppContext) {
    let mut sizes = Vec::new();
    for base in [16., 24.] {
        let (harness, ids) = setup_zoomed(cx, base);
        let trailing = harness.ui(cx, |window, _cx| {
            f32::from(window.find("trailing-space").bounds().size.height)
        });
        let measured = cx.update(|cx| {
            let editor = harness.editor.read(cx);
            let heading = editor.block_bounds(ids[0]).expect("the heading laid out");
            let paragraph = editor.block_bounds(ids[1]).expect("the paragraph laid out");
            // The marker sits inside the block, so the distance between a
            // paragraph's text and a list item's is measured at the glyphs.
            let prose = editor.input_geometry(ids[1], cx).expect("prose laid out");
            let item = editor.input_geometry(ids[2], cx).expect("item laid out");
            (
                f32::from(heading.size.height),
                f32::from(paragraph.origin.y - heading.origin.y),
                item.4 - prose.4,
                trailing,
            )
        });
        sizes.push(measured);
    }

    let (small, large) = (sizes[0], sizes[1]);
    let ratio = 24. / 16.;
    for (a, b, what) in [
        (small.0, large.0, "the heading's height"),
        (small.1, large.1, "the space under the heading"),
        (small.2, large.2, "the indent of a list item"),
        (small.3, large.3, "the space under the last block"),
    ] {
        assert!(
            (b / a - ratio).abs() < 0.12,
            "{what} went from {a} to {b}, which is not the {ratio}x the base changed by"
        );
    }
}

#[gpui_kit::test]
fn a_zoomed_document_still_holds_its_text(cx: &mut TestAppContext) {
    let (harness, ids) = setup_zoomed(cx, 24.);
    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        for id in &ids {
            assert!(
                editor.text_area_fits_text(*id, cx),
                "a block came up short at a bigger base size"
            );
        }
    });
}

#[gpui_kit::test]
fn an_application_can_restyle_the_editor(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let before = cx.update(|cx| f32::from(EditorTheme::global(cx).page_width));

    cx.update(|cx| {
        EditorTheme::customize(cx, |theme, _| {
            theme.page_width = theme.rems(30.);
            theme.comment_fill = gpui_kit::red();
        });
    });
    harness.ui(cx, |window, cx| window.render_frame(cx));

    let after = cx.update(|cx| f32::from(EditorTheme::global(cx).page_width));
    assert_ne!(before, after);
    assert_eq!(after, 480.);

    // A theme change re-derives the tokens; the customization survives it.
    cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
    harness.ui(cx, |window, cx| window.render_frame(cx));
    cx.update(|cx| {
        let theme = EditorTheme::global(cx);
        assert_eq!(f32::from(theme.page_width), 480.);
        assert_eq!(theme.comment_fill, gpui_kit::red());
    });
}

#[gpui_kit::test]
fn switching_to_dark_repaints_the_document(cx: &mut TestAppContext) {
    let harness = setup(cx);
    let light = cx.update(|cx| EditorTheme::global(cx).code_foreground);

    cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
    harness.ui(cx, |window, cx| window.render_frame(cx));

    cx.update(|cx| {
        let theme = EditorTheme::global(cx);
        assert_ne!(theme.code_foreground, light, "the palette did not follow");
        assert_eq!(
            theme.code_block_background.a,
            gpui_kit::component::Theme::global(cx).muted.opacity(0.45).a,
            "a surface kept a light-theme value"
        );
    });
}

#[gpui_kit::test]
fn the_zoom_keys_resize_the_document(cx: &mut TestAppContext) {
    let harness = setup(cx);
    cx.update(editor::theme::init_appearance_actions);
    harness.type_text("prose", cx);

    let measure = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let editor = harness.editor.read(cx);
            let id = editor.block_id_at(0).unwrap();
            f32::from(editor.block_bounds(id).unwrap().size.height)
        })
    };
    let before = measure(cx);

    harness.press("secondary-=", cx);
    harness.ui(cx, |window, cx| window.render_frame(cx));
    let bigger = measure(cx);
    assert!(
        bigger > before,
        "the block did not grow with the base size ({before} → {bigger})"
    );
    assert_eq!(cx.update(|cx| f32::from(Theme::global(cx).font_size)), 18.);

    harness.press("secondary-0", cx);
    harness.ui(cx, |window, cx| window.render_frame(cx));
    assert_eq!(measure(cx), before, "the reset did not put the size back");

    // The appearance action swaps the palette without losing the size.
    cx.update(|cx| editor::theme::set_base_size(20., cx));
    harness.ui(cx, |window, cx| window.render_frame(cx));
    harness.press("secondary-shift-l", cx);
    harness.ui(cx, |window, cx| window.render_frame(cx));
    cx.update(|cx| {
        assert!(Theme::global(cx).mode.is_dark());
        assert_eq!(f32::from(Theme::global(cx).font_size), 20.);
        assert_eq!(f32::from(EditorTheme::global(cx).rem), 20.);
    });
}

#[gpui_kit::test]
fn the_template_palette_survives_an_appearance_change(cx: &mut TestAppContext) {
    let harness = setup(cx);
    cx.update(|cx| {
        editor::theme::apply_template_palette(cx).expect("the template palette loaded")
    });
    harness.ui(cx, |window, cx| window.render_frame(cx));

    cx.update(|cx| {
        let kit = Theme::global(cx);
        assert!(!kit.mode.is_dark());
        assert_eq!(kit.background, gpui_kit::rgb(0xffffff).into(), "page");
        assert_eq!(
            EditorTheme::global(cx).highlight_fill(Some(gpui_notion::editor::HighlightColor::Yellow)),
            gpui_kit::rgb(0xfef9c3).into(),
            "the template's yellow highlight"
        );
    });

    cx.update(editor::theme::toggle_appearance);
    harness.ui(cx, |window, cx| window.render_frame(cx));

    cx.update(|cx| {
        let kit = Theme::global(cx);
        assert!(kit.mode.is_dark());
        assert_eq!(kit.background, gpui_kit::rgb(0x0e0e11).into(), "dark page");
        assert_eq!(
            EditorTheme::global(cx).highlight_fill(Some(gpui_notion::editor::HighlightColor::Yellow)),
            gpui_kit::rgb(0x6b6524).into(),
            "the template's dark yellow highlight"
        );
    });
}

#[gpui_kit::test]
fn the_document_settles_and_stops_moving(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("a paragraph that sits still", cx);

    let mut frames = Vec::new();
    for _ in 0..12 {
        harness.ui(cx, |window, cx| window.render_frame(cx));
        frames.push(cx.update(|cx| {
            let editor = harness.editor.read(cx);
            let id = editor.block_id_at(0).unwrap();
            editor.input_geometry(id, cx)
        }));
    }
    let settled: Vec<_> = frames[6..].to_vec();
    assert!(
        settled.windows(2).all(|pair| pair[0] == pair[1]),
        "the block never stopped moving: {settled:#?}"
    );
}

#[gpui_kit::test]
fn guest_toolbar_dispatches_only_declared_actions_without_changing_document(
    cx: &mut TestAppContext,
) {
    use gpui_notion::editor::toolbar::{ToolbarAction, ToolbarItem};
    let harness = setup(cx);
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&harness.editor, move |_, action: &ToolbarAction, _| {
            observed.borrow_mut().push(action.tag.to_string())
        })
    });
    harness.ui(cx, |_, cx| {
        harness.editor.update(cx, |editor, cx| {
            let before = editor.content();
            editor.set_toolbar(
                Some(vec![ToolbarItem {
                    tag: "custom-action".into(),
                    label: "Review selection".into(),
                }]),
                cx,
            );
            editor.choose_toolbar_action("missing", cx);
            editor.choose_toolbar_action("custom-action", cx);
            assert_eq!(editor.content(), before);
        })
    });
    assert_eq!(&*events.borrow(), &["custom-action"]);
}

#[gpui_kit::test]
fn external_annotations_emit_selection_without_private_threads(cx: &mut TestAppContext) {
    use gpui_notion::editor::comments::{AnnotationMode, AnnotationRequested};
    let harness = setup(cx);
    harness.type_text("hello world", cx);
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&harness.editor, move |_, event: &AnnotationRequested, _| {
            observed.borrow_mut().push(event.clone())
        })
    });
    harness.ui(cx, |window, cx| {
        harness.editor.update(cx, |editor, cx| {
            editor.set_annotation_mode(AnnotationMode::External);
            editor.select_text_in_block(0, 0..5, window, cx);
            let before = editor.content();
            editor.add_comment(window, cx);
            assert_eq!(editor.content(), before);
            assert!(!editor.comment_draft_is_open());
        })
    });
    assert_eq!(events.borrow().len(), 1);
    assert_eq!(events.borrow()[0].range, 0..5);
}

#[gpui_kit::test]
fn application_suggestions_do_not_run_native_trigger_or_replacement_rules(cx: &mut TestAppContext) {
    use gpui_notion::editor::slash::{ApplicationMenu, ApplicationMenuAnchor, MenuAction};
    use gpui_notion::editor::toolbar::ToolbarItem;
    let harness = setup(cx);
    harness.ui(cx, |_, cx| {
        harness
            .editor
            .update(cx, |editor, cx| editor.set_application_menu(None, cx))
    });
    harness.type_text("@", cx);
    assert!(
        !cx.update(|cx| harness.editor.read(cx).suggestion_is_open()),
        "the application owns trigger detection"
    );
    let actions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = actions.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&harness.editor, move |_, action: &MenuAction, _| {
            captured.lock().unwrap().push(action.clone());
        })
    });
    harness.ui(cx, |window, cx| window.click(("block", 1usize), cx));
    assert!(
        actions.lock().unwrap().is_empty(),
        "a closed menu has nothing to dismiss"
    );
    harness.ui(cx, |_, cx| {
        harness.editor.update(cx, |editor, cx| {
            editor.set_application_menu(
                Some(ApplicationMenu {
                    anchor: ApplicationMenuAnchor::Caret,
                    items: vec![ToolbarItem {
                        tag: "opaque-action".into(),
                        label: "Application choice".into(),
                    }],
                    selected: 0,
                }),
                cx,
            )
        })
    });
    assert!(cx.update(|cx| harness.editor.read(cx).suggestion_is_open()));
    harness.press("enter", cx);
    assert_eq!(
        harness.texts(cx),
        vec!["@"],
        "a menu pick never replaces source text"
    );
    assert_eq!(
        *actions.lock().unwrap(),
        vec![MenuAction::Pick("opaque-action".into())]
    );
}

#[gpui_kit::test]
fn caret_changes_are_observed_without_turning_repaints_into_edits(cx: &mut TestAppContext) {
    use gpui_notion::editor::view::{DocumentChanged, SelectionChanged};
    let harness = setup(cx);
    harness.type_text("hello", cx);
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let selections = events.clone();
    let documents = events.clone();
    let _selection = cx.update(|cx| {
        cx.subscribe(&harness.editor, move |_, _: &SelectionChanged, _| {
            selections.borrow_mut().push("selection");
        })
    });
    let _document = cx.update(|cx| {
        cx.subscribe(&harness.editor, move |_, _: &DocumentChanged, _| {
            documents.borrow_mut().push("document");
        })
    });
    harness.press("left", cx);
    assert_eq!(*events.borrow(), vec!["selection"]);
    harness.ui(cx, |_, _| {});
    assert_eq!(
        *events.borrow(),
        vec!["selection"],
        "a repaint changes no selection"
    );
    harness.press("shift-left", cx);
    assert_eq!(*events.borrow(), vec!["selection", "selection"]);
    assert_eq!(harness.texts(cx), vec!["hello"]);
}

#[gpui_kit::test]
fn application_input_rules_leave_typed_and_pasted_source_unchanged(cx: &mut TestAppContext) {
    use gpui_notion::editor::input_rules::InputRuleMode;
    for source in ["# Title", "**bold**", "(c)"] {
        let harness = setup(cx);
        harness.ui(cx, |_, cx| {
            harness.editor.update(cx, |editor, _| {
                editor.set_input_rule_mode(InputRuleMode::Application);
            })
        });
        harness.type_text(source, cx);
        assert_eq!(
            harness.texts(cx).join("\n"),
            source,
            "source belongs to the application"
        );
        assert!(harness.types(cx).iter().all(|kind| kind == "paragraph"));
        cx.update(|cx| {
            assert!(
                harness.editor.read(cx).content().iter().all(|block| block
                    .marks
                    .iter()
                    .next()
                    .is_none())
            )
        });
    }
    let harness = setup(cx);
    harness.ui(cx, |_, cx| {
        harness.editor.update(cx, |editor, _| {
            editor.set_input_rule_mode(InputRuleMode::Application);
        })
    });
    harness.ui(cx, |window, cx| window.input("first\n# second", cx));
    assert_eq!(harness.texts(cx), vec!["first", "# second"]);
    assert_eq!(harness.types(cx), vec!["paragraph", "paragraph"]);
}

#[gpui_kit::test]
fn a_block_names_its_own_placeholder(cx: &mut TestAppContext) {
    use gpui_notion::editor::block::BlockAttrs;

    let harness = setup(cx);
    harness.ui(cx, |window, cx| {
        harness.editor.update(cx, |editor, cx| {
            let mut attrs = BlockAttrs::level(1);
            attrs
                .extra
                .insert("placeholder".into(), "Untitled".into());
            let named = BlockContent::new(types::HEADING, "").with_attrs(attrs);
            editor.insert_block(0, named, window, cx);
            let plain = BlockContent::new(types::HEADING, "").with_attrs(BlockAttrs::level(1));
            editor.insert_block(1, plain, window, cx);
        })
    });

    cx.update(|cx| {
        let editor = harness.editor.read(cx);
        assert_eq!(editor.placeholder_at(0, cx).as_ref(), "Untitled");
        assert_eq!(editor.placeholder_at(1, cx).as_ref(), "Heading 1");
    });
}

#[gpui_kit::test]
fn the_gutter_stays_with_the_menu_it_opened(cx: &mut TestAppContext) {
    let harness = setup(cx);
    harness.type_text("one", cx);
    harness.press("enter", cx);
    harness.type_text("two", cx);

    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(("block", 1usize), cx);
        window.render_frame(cx);
        window.click(("drag", 1usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    let holder = cx.update(|cx| harness.editor.read(cx).gutter_menu_for_test());
    assert_eq!(holder, Some(BlockId(1)), "the handle's block holds the menu");

    // The pointer walking to another line must not take the open menu's own
    // controls away — that is the whole defect.
    cx.update_window(harness.window, |_, window, cx| {
        window.hover(("block", 2usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();

    assert_eq!(
        cx.update(|cx| harness.editor.read(cx).gutter_menu_for_test()),
        Some(BlockId(1)),
        "hovering elsewhere does not close the menu"
    );
}
