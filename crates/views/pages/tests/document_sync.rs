//! The page document's text <-> block translation and its save plan, held to
//! the fixtures they were written against. This is the pure half of the pages
//! view: the guest reads the node's blocks, renders them as one markdown
//! buffer, and diffs an edited buffer back into the module's own ops.

use pages_view::document_sync::*;

fn line(kind: &str, text: &str) -> Line {
    Line {
        kind: kind.into(),
        text: text.into(),
        checked: false,
        depth: 0,
    }
}

fn line_at(depth: usize, text: &str) -> Line {
    Line {
        depth,
        ..line("Text", text)
    }
}

fn stored(id: &str, kind: &str, text: &str) -> StoredLine {
    StoredLine {
        id: id.into(),
        has_children: false,
        line: line(kind, text),
    }
}

#[test]
fn a_stored_number_run_reopens_counting_up() {
    let blocks = [
        block("Number", "one", 0),
        block("Number", "child", 1),
        block("Number", "sibling", 1),
        block("Number", "two", 0),
        block("Text", "plain", 0),
        block("Number", "apart", 0),
    ];
    assert_eq!(
        page_markdown(&blocks),
        "1. one\n  1. child\n  2. sibling\n2. two\nplain\n1. apart"
    );
    // …and the reopened text is what the parser reads back.
    assert_eq!(
        parse_document(&page_markdown(&blocks))
            .iter()
            .map(|line| line.kind.clone())
            .collect::<Vec<_>>(),
        vec!["Number", "Number", "Number", "Number", "Text", "Number"]
    );
}

#[test]
fn a_hundred_item_run_still_reads_back_as_a_list_and_a_year_does_not() {
    assert_eq!(
        parse_document("100. hundredth")[0],
        line("Number", "hundredth")
    );
    assert_eq!(
        parse_document("1997. A great year")[0],
        line("Text", "1997. A great year")
    );
}

#[test]
fn every_block_kind_survives_a_round_trip() {
    let kinds = [
        ("Text", "plain"),
        ("Heading 1", "one"),
        ("Heading 2", "two"),
        ("Heading 3", "three"),
        ("Bullet", "point"),
        ("Number", "first"),
        ("Toggle", "fold"),
        ("Quote", "said"),
        ("Callout", "note"),
        ("Divider", ""),
        ("Code", "let x = 1;"),
    ];
    for (kind, text) in kinds {
        let source = line(kind, text);
        let rendered = render_line(&source, 1);
        let parsed = parse_document(&rendered);
        assert_eq!(
            parsed,
            vec![source],
            "{kind} did not round-trip: {rendered}"
        );
    }
}

#[test]
fn a_ticked_todo_keeps_its_tick_through_the_round_trip() {
    let done = Line {
        checked: true,
        ..line("Todo", "shipped")
    };
    assert_eq!(render_line(&done, 1), "- [x] shipped");
    assert_eq!(parse_document("- [x] shipped"), vec![done]);
}

#[test]
fn an_empty_line_is_an_empty_text_block_and_round_trips() {
    // Enter-Enter — the most ordinary key in a document — makes a blank
    // paragraph. It must be writable, not a save error.
    assert_eq!(parse_document("one\n\ntwo").len(), 3);
    assert_eq!(parse_document("one\n\ntwo")[1], line("Text", ""));
    assert_eq!(render_line(&line("Text", ""), 1), "");
}

#[test]
fn a_code_body_keeps_its_trailing_newline_through_the_round_trip() {
    let code = line("Code", "x\n");
    let rendered = render_line(&code, 1);
    assert_eq!(rendered, "```\nx\n\n```");
    assert_eq!(parse_document(&rendered), vec![code]);
}

#[test]
fn an_open_fence_is_flagged_until_its_close_arrives() {
    assert!(has_unclosed_fence("Title\n```\nlet x = 1;"));
    assert!(!has_unclosed_fence("Title\n```\nlet x = 1;\n```"));
    // Line 0 is the title, never parsed — backticks there do not count.
    assert!(!has_unclosed_fence("``` in a title\nbody"));
    assert!(!has_unclosed_fence(""));
}

#[test]
fn a_multi_line_code_block_folds_back_into_one_block() {
    let code = line("Code", "fn main() {\n    go();\n}");
    let rendered = render_line(&code, 1);
    assert_eq!(rendered, "```\nfn main() {\n    go();\n}\n```");
    // The body's OWN indentation is content, not layout — a round trip
    // that reformats it has eaten the user's code.
    assert_eq!(parse_document(&rendered), vec![code]);
}

#[test]
fn a_nested_code_block_loses_its_nesting_indent_and_keeps_its_own() {
    let parent = line("Bullet", "setup");
    let code = Line {
        depth: 1,
        ..line("Code", "if x:\n    go()")
    };
    let rendered = format!("{}\n{}", render_line(&parent, 1), render_line(&code, 1));
    assert_eq!(rendered, "- setup\n  ```\n  if x:\n      go()\n  ```");
    assert_eq!(parse_document(&rendered), vec![parent, code]);
}

#[test]
fn markdown_inside_a_fence_is_code_not_a_heading() {
    let parsed = parse_document("```\n# not a heading\n```");
    assert_eq!(parsed, vec![line("Code", "# not a heading")]);
}

#[test]
fn depth_round_trips_as_two_spaces_per_level() {
    let ladder = "- a\n  - b\n    - c";
    let parsed = parse_document(ladder);
    assert_eq!(
        parsed.iter().map(|l| l.depth).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        parsed
            .iter()
            .map(|line| render_line(line, 1))
            .collect::<Vec<_>>()
            .join("\n"),
        ladder
    );
}

#[test]
fn depth_is_clamped_to_what_the_tree_can_hold() {
    // An isolated deep line has nothing to nest under: the tree cannot
    // represent it, and an unclamped depth becomes a MoveBlock the module
    // rejects on every tick, forever.
    assert_eq!(parse_document("    - deep")[0].depth, 0);
    let jump = parse_document("- a\n        - way deep");
    assert_eq!(jump[1].depth, 1);
}

#[test]
fn tabs_count_as_indent_steps_and_an_odd_space_stays_in_the_text() {
    let tabbed = parse_document("- a\n\t- b");
    assert_eq!(
        tabbed[1],
        Line {
            depth: 1,
            ..line("Bullet", "b")
        }
    );
    // Three spaces: one step, and the odd space belongs to the text.
    assert_eq!(split_indent("   x"), (1, " x"));
    assert_eq!(parse_document("- a\n   x")[1], line_at(1, " x"));
}

#[test]
fn a_long_number_is_prose_because_its_digits_would_be_destroyed() {
    // The stored ordered marker is positional — "1997." would come back
    // as "1.", deleting the year. Two digits keep real lists working.
    assert_eq!(
        parse_document("1997. A great year")[0],
        line("Text", "1997. A great year")
    );
    assert_eq!(parse_document("12. twelfth")[0], line("Number", "twelfth"));
}

#[test]
fn a_final_empty_line_is_a_block_and_survives_the_round_trip() {
    let parsed = parse_document("one\n");
    assert_eq!(parsed, vec![line("Text", "one"), line("Text", "")]);
    assert_eq!(parse_document(""), Vec::new());
}

#[test]
fn an_untouched_document_writes_nothing() {
    let have = vec![stored("a", "Text", "one"), stored("b", "Text", "two")];
    let want = vec![line("Text", "one"), line("Text", "two")];
    assert_eq!(document_plan(&have, &want).ops, Vec::new());
}

#[test]
fn editing_one_line_updates_only_that_block() {
    let have = vec![
        stored("a", "Text", "one"),
        stored("b", "Text", "two"),
        stored("c", "Text", "three"),
    ];
    let want = vec![
        line("Text", "one"),
        line("Text", "TWO"),
        line("Text", "three"),
    ];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::SetText {
            id: "b".into(),
            text: "TWO".into(),
        }]
    );
}

#[test]
fn typing_a_hash_promotes_the_block_in_place_and_keeps_its_id() {
    let have = vec![stored("a", "Text", "Title")];
    let want = vec![line("Heading 1", "Title")];
    // Only the KIND moved, so only one write is billed.
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::SetKind {
            id: "a".into(),
            kind: "Heading 1".into(),
        }]
    );
}

#[test]
fn indenting_a_line_moves_it_one_step_rather_than_reparenting_it() {
    let have = vec![stored("a", "Bullet", "one"), stored("b", "Bullet", "two")];
    let want = vec![
        line("Bullet", "one"),
        Line {
            depth: 1,
            ..line("Bullet", "two")
        },
    ];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::Nest {
            id: "b".into(),
            direction: "indent".into(),
        }]
    );
}

#[test]
fn outdenting_is_the_same_move_in_the_other_direction() {
    let have = vec![StoredLine {
        line: Line {
            depth: 2,
            ..line("Bullet", "deep")
        },
        ..stored("a", "Bullet", "deep")
    }];
    let want = vec![Line {
        depth: 1,
        ..line("Bullet", "deep")
    }];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::Nest {
            id: "a".into(),
            direction: "outdent".into(),
        }]
    );
}

#[test]
fn ticking_a_todo_writes_only_the_tick() {
    let have = vec![stored("a", "Todo", "ship it")];
    let want = vec![Line {
        checked: true,
        ..line("Todo", "ship it")
    }];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::SetChecked {
            id: "a".into(),
            checked: true,
        }]
    );
}

#[test]
fn a_new_middle_line_anchors_on_the_block_above_it() {
    let have = vec![stored("a", "Text", "one"), stored("b", "Text", "three")];
    let want = vec![
        line("Text", "one"),
        line("Text", "two"),
        line("Text", "three"),
    ];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::Insert {
            after: "a".into(),
            kind: "Text".into(),
            text: "two".into(),
        }]
    );
}

#[test]
fn a_deleted_line_removes_exactly_its_own_block() {
    let have = vec![
        stored("a", "Text", "one"),
        stored("b", "Text", "two"),
        stored("c", "Text", "three"),
    ];
    let want = vec![line("Text", "one"), line("Text", "three")];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::Remove { id: "b".into() }]
    );
}

#[test]
fn removing_a_parent_is_refused_when_its_subtree_survives() {
    let parent = StoredLine {
        has_children: true,
        ..stored("b", "Text", "parent of things")
    };
    let child = StoredLine {
        line: line_at(1, "kept child"),
        ..stored("c", "Text", "kept child")
    };
    let have = vec![stored("a", "Text", "one"), parent, child.clone()];
    let want = vec![line("Text", "one"), child.line.clone()];
    let plan = document_plan(&have, &want);
    assert!(plan.ops.is_empty(), "a refused plan writes nothing");
    assert!(plan.refusal.contains("still has sub-items"), "{plan:?}");
}

#[test]
fn deleting_a_whole_subtree_together_is_allowed_leaves_first() {
    let parent = StoredLine {
        has_children: true,
        ..stored("b", "Text", "parent")
    };
    let child = StoredLine {
        line: line_at(1, "child"),
        ..stored("c", "Text", "child")
    };
    let have = vec![stored("a", "Text", "one"), parent, child];
    let want = vec![line("Text", "one")];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![
            BlockOp::Remove { id: "c".into() },
            BlockOp::Remove { id: "b".into() },
        ]
    );
}

#[test]
fn an_unperformable_indent_is_deferred_not_submitted() {
    // The first body line has no previous sibling; the module would
    // reject the MoveBlock forever. An empty plan lets the baseline
    // settle instead.
    let have = vec![stored("a", "Bullet", "one")];
    let want = vec![Line {
        depth: 1,
        ..line("Bullet", "one")
    }];
    assert_eq!(document_plan(&have, &want).ops, Vec::new());
}

#[test]
fn unticking_a_todo_by_retyping_its_kind_writes_no_phantom_tick() {
    let done = StoredLine {
        line: Line {
            checked: true,
            ..line("Todo", "was done")
        },
        ..stored("a", "Todo", "was done")
    };
    let want = vec![line("Bullet", "was done")];
    // Only the kind moves — SetChecked is Todo-only on the node, and a
    // Bullet has no tick to reconcile.
    assert_eq!(
        document_plan(&[done], &want).ops,
        vec![BlockOp::SetKind {
            id: "a".into(),
            kind: "Bullet".into(),
        }]
    );
}

#[test]
fn appending_to_the_end_anchors_on_the_last_stored_block() {
    let have = vec![stored("a", "Text", "one")];
    let want = vec![line("Text", "one"), line("Bullet", "next")];
    assert_eq!(
        document_plan(&have, &want).ops,
        vec![BlockOp::Insert {
            after: "a".into(),
            kind: "Bullet".into(),
            text: "next".into(),
        }]
    );
}

#[test]
fn the_first_line_of_an_empty_page_anchors_on_nothing() {
    let want = vec![line("Text", "hello")];
    assert_eq!(
        document_plan(&[], &want).ops,
        vec![BlockOp::Insert {
            after: String::new(),
            kind: "Text".into(),
            text: "hello".into(),
        }]
    );
}

fn block(kind: &str, text: &str, depth: usize) -> PageBlock {
    PageBlock {
        key: 0,
        id: text.into(),
        parent: String::new(),
        kind: kind.into(),
        text: text.into(),
        checked: false,
        prefix: INDENT.repeat(depth),
        child_count: 0,
    }
}

#[test]
fn the_title_is_line_zero_and_the_body_starts_under_it() {
    let blocks = [block("Text", "first", 0), block("Bullet", "second", 0)];
    let text = page_document_text("My page", &blocks);
    assert_eq!(text, "My page\nfirst\n- second");
    assert_eq!(document_title(&text), "My page");
    assert_eq!(
        document_body(&text),
        vec![line("Text", "first"), line("Bullet", "second")]
    );
}

#[test]
fn a_title_only_page_opens_with_a_body_line_to_type_into() {
    // This asserted `"Just a title"` with no trailing newline, on the
    // grounds that the blank line was stray. It was not stray, it was the
    // only place the caret could go: with a one-line buffer a click in the
    // empty space below the heading leaves the caret at the end of line 0,
    // so the first words typed on a new page are appended to its title.
    let text = page_document_text("Just a title", &[]);
    assert_eq!(text, "Just a title\n");
    assert_eq!(document_title(&text), "Just a title");
    // The line is in the BUFFER only — the document still holds no blocks,
    // so opening a page and leaving it alone writes nothing.
    assert_eq!(document_body(&text), Vec::new());
}

/// THE DEFECT THE BODY LINE EXISTS FOR. Typing on a fresh page must reach
/// the body, not the heading. With a one-line buffer the caret has nowhere
/// else to be, so the sentence lands on line 0 and `document_title` — which
/// is simply `lines().next()` — reports the page renamed.
#[test]
fn the_first_words_typed_on_a_new_page_are_not_its_title() {
    let opened = page_document_text("Untitled note", &[]);
    // What an editor does with a caret on the last line: append there.
    let typed = format!("{opened}hello from the body");

    assert_eq!(
        document_title(&typed),
        "Untitled note",
        "typing in the body must not rename the page"
    );
    let body = document_body(&typed);
    assert_eq!(body.len(), 1, "the sentence is a body line of its own");
    assert!(format!("{body:?}").contains("hello from the body"));
}

#[test]
fn markdown_on_the_title_line_stays_literal_because_a_title_has_no_kind() {
    // `# ` on line 0 is part of the title's own text, not a heading marker:
    // `document_body` never sees line 0, so nothing can parse it.
    let text = "# Not a heading\nbody";
    assert_eq!(document_title(text), "# Not a heading");
    assert_eq!(document_body(text), vec![line("Text", "body")]);
}

#[test]
fn emptying_the_buffer_leaves_an_empty_title_and_no_blocks() {
    assert_eq!(document_title(""), "");
    assert_eq!(document_body(""), Vec::new());
}

#[test]
fn line_spans_mirror_the_rendered_document_and_skip_subpages() {
    let blocks = [
        block("Heading 1", "title-ish", 0),
        block("Text", "para", 0),
        block("Page", "a subpage", 0),
        block("Code", "a\nb", 0),
        block("Todo", "ship", 0),
    ];
    // Line 0 is the page title; the code block is fence+2 body+fence.
    assert_eq!(
        line_spans(&blocks),
        vec![
            ("title-ish".into(), 1, 1),
            ("para".into(), 2, 1),
            ("a\nb".into(), 3, 4),
            ("ship".into(), 7, 1),
        ]
    );
    assert_eq!(block_at_line(&blocks, 0), "");
    assert_eq!(block_at_line(&blocks, 2), "para");
    assert_eq!(block_at_line(&blocks, 5), "a\nb");
    assert_eq!(block_at_line(&blocks, 7), "ship");
    assert_eq!(block_at_line(&blocks, 8), "");
}

#[test]
fn subpages_are_not_prose_and_never_reach_the_document() {
    assert!(!is_prose(&block("Page", "a child page", 0)));
    assert!(is_prose(&block("Text", "prose", 0)));
    let blocks = [
        block("Text", "before", 0),
        block("Page", "a child page", 0),
        block("Text", "after", 0),
    ];
    assert_eq!(page_markdown(&blocks), "before\nafter");
    assert_eq!(stored_lines(&blocks).len(), 2);
}

#[test]
fn a_page_s_blocks_render_as_the_document_the_editor_opens_on() {
    let blocks = [
        block("Heading 1", "Title", 0),
        block("Text", "A paragraph.", 0),
        block("Bullet", "first", 0),
        block("Bullet", "nested", 1),
    ];
    assert_eq!(
        page_markdown(&blocks),
        "# Title\nA paragraph.\n- first\n  - nested"
    );
    // ...and the document reads back as the same lines it was built from.
    assert_eq!(
        parse_document(&page_markdown(&blocks)),
        stored_lines(&blocks)
            .into_iter()
            .map(|stored| stored.line)
            .collect::<Vec<_>>()
    );
}

/// A selection's byte columns become the module's UTF-16 anchor over the
/// block's own text: the marker is not part of the block, and a selection
/// that never leaves the marker anchors nothing.
#[test]
fn a_text_anchor_counts_utf16_units_past_the_block_marker() {
    assert_eq!(text_anchor("- [ ] 한글 words", Some(6..12)), Some((0, 2)));
    assert_eq!(text_anchor("## 👍🏽 ok", Some(12..14)), Some((5, 7)));
    assert_eq!(text_anchor("## head", Some(0..2)), None);
    assert_eq!(text_anchor("## head", Some(1..5)), Some((0, 2)));
    assert_eq!(text_anchor("plain", Some(3..3)), None);
    assert_eq!(text_anchor("plain", None), None);
}
