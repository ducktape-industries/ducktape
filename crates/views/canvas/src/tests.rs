use super::*;
fn view() -> BoardsView {
    let (mut view, _) = BoardsView::boot();
    view.current = "room".into();
    view.confirmed = Some(Board::new("Planning".into(), "owner".into()).unwrap());
    view
}
fn card(view: &mut BoardsView, id: &str, x: i32) -> Task<Message> {
    view.edit(Change::Create {
        id: id.into(),
        shape: Shape {
            x,
            ..Default::default()
        },
    })
}
fn segment(kind: Kind) -> Shape {
    Shape {
        kind,
        width: 160,
        height: 90,
        points: vec![[0, 0], [160, 90]],
        ..Default::default()
    }
}
/// Press, drag through the given screen points, release.
fn drag(view: &mut BoardsView, path: &[[f32; 2]]) {
    let [first, rest @ ..] = path else { return };
    view.on_press(first[0], first[1]);
    for step in rest {
        view.on_move(step[0], step[1]);
    }
    view.on_release();
}
#[test]
fn optimistic_edits_remain_visible_while_waiting_for_consensus() {
    let mut view = view();
    card(&mut view, "a", 0);
    view.edit(Change::Move {
        id: "a".into(),
        x: 250,
        y: 40,
    });
    view.edit(Change::Text {
        id: "a".into(),
        text: "공유 캔버스".into(),
    });
    assert!(view.confirmed.as_ref().unwrap().shapes.is_empty());
    let board = view.visible().unwrap();
    assert_eq!(board.shapes["a"].shape.x, 250);
    assert_eq!(board.shapes["a"].shape.text, "공유 캔버스");
    assert_eq!(view.pending.len(), 3);
}
#[test]
fn remote_change_is_rebased_under_pending_local_fields() {
    let mut view = view();
    let board = view
        .confirmed
        .take()
        .unwrap()
        .changed(&Change::Create {
            id: "a".into(),
            shape: Shape::default(),
        })
        .unwrap();
    view.confirmed = Some(board.clone());
    view.edit(Change::Move {
        id: "a".into(),
        x: 500,
        y: 50,
    });
    let remote = board
        .changed(&Change::Text {
            id: "a".into(),
            text: "Remote text".into(),
        })
        .unwrap();
    view.on_read(
        0,
        "room".into(),
        Ok(host::Reading {
            catalog: BTreeMap::new(),
            board: Some(remote),
        }),
    );
    let board = view.visible().unwrap();
    assert_eq!(board.shapes["a"].shape.x, 500);
    assert_eq!(board.shapes["a"].shape.text, "Remote text");
}
#[test]
fn acknowledgement_does_not_invent_a_revision_and_failed_saves_keep_drafts() {
    let mut view = view();
    card(&mut view, "a", 0);
    let committed = view.visible().unwrap();
    view.confirmed = Some(committed.clone());
    view.on_delivered(
        0,
        "room".into(),
        Ok(()),
        Ok(host::Reading {
            catalog: BTreeMap::new(),
            board: Some(committed.clone()),
        }),
    );
    assert_eq!(
        view.confirmed.as_ref().unwrap().revision,
        committed.revision
    );
    view.edit(Change::Text {
        id: "a".into(),
        text: "Keep me".into(),
    });
    view.on_delivered(
        0,
        "room".into(),
        Err("offline".into()),
        Err("offline".into()),
    );
    assert_eq!(view.pending.len(), 1);
    assert_eq!(view.visible().unwrap().shapes["a"].shape.text, "Keep me");
}
#[test]
fn zoom_preserves_anchor_and_drag_commits_world_coordinates() {
    let mut view = view();
    card(&mut view, "a", 0);
    let center = [view.viewport[0] / 2., view.viewport[1] / 2.];
    let before = view.world(center);
    view.on_zoom(1.25);
    assert_eq!(view.world(center), before);
    view.camera = [80., 80.];
    view.zoom = 2.;
    view.on_press(100., 100.);
    view.on_move(160., 140.);
    assert_eq!(view.visible().unwrap().shapes["a"].shape.x, 30);
    view.on_release();
    assert_eq!(view.visible().unwrap().shapes["a"].shape.y, 20);
}
#[test]
fn undo_delete_restores_attached_arrows_and_snapshot_keeps_pending_work() {
    let mut view = view();
    card(&mut view, "a", 0);
    card(&mut view, "b", 300);
    view.edit(Change::Create {
        id: "edge".into(),
        shape: Shape {
            from: Some("a".into()),
            to: Some("b".into()),
            ..segment(Kind::Arrow)
        },
    });
    view.selected = ["a".into()].into();
    view.on_delete();
    assert_eq!(view.visible().unwrap().shapes.len(), 1);
    view.on_undo();
    assert_eq!(view.visible().unwrap().shapes.len(), 3);
    let restored = BoardsView::restore(&view.snapshot().unwrap()).unwrap();
    assert_eq!(restored.visible(), view.visible());
    assert_eq!(restored.pending.len(), view.pending.len());
}
#[test]
fn native_wire_tree_uses_canvas_and_text_input_within_frame_budget() {
    let mut view = view();
    let mut board = view.confirmed.take().unwrap();
    for i in 0..boards::MAX_SHAPES {
        board = board
            .changed(&Change::Create {
                id: format!("card-{i}"),
                shape: Shape {
                    x: (i as i32 % 8) * 220,
                    text: "한글".repeat(300),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    view.confirmed = Some(board);
    view.selected = ["card-0".into()].into();
    let mut driver =
        ducktape_view_guest::Driver::<BoardsView>::from_snapshot(&view.snapshot().unwrap(), false)
            .unwrap();
    let mut frame = driver.tick(Vec::new());
    assert!(
        !ducktape_view_guest::wire::sanitize(&mut frame)
            .unwrap()
            .display_text_truncated
    );
    let tree = frame.root.unwrap();
    let bytes = serde_json::to_vec(&tree).unwrap();
    assert!(bytes.len() < 1_000_000);
    let json = String::from_utf8(bytes).unwrap();
    assert!(json.contains("Canvas"));
    assert!(json.contains("boards/tool"));
}

#[test]
fn reconnect_keeps_an_inflight_receipt_in_the_same_network() {
    let mut view = view();
    view.session.chain = "network".into();
    view.session.connected = true;
    card(&mut view, "a", 0);
    assert!(matches!(view.delivery, Delivery::Sending));
    view.on_session(Ok(host::Session {
        connected: false,
        dark: false,
        chain: "network".into(),
    }));
    view.on_session(Ok(host::Session {
        connected: true,
        dark: false,
        chain: "network".into(),
    }));
    assert_eq!(view.epoch, 0);
    let committed = view.visible().unwrap();
    view.on_delivered(
        0,
        "room".into(),
        Ok(()),
        Ok(host::Reading {
            catalog: BTreeMap::new(),
            board: Some(committed),
        }),
    );
    assert!(view.pending.is_empty());
}

fn key(
    view: &mut BoardsView,
    key: wire::keyboard::Key,
    modifiers: wire::keyboard::Modifiers,
    captured: bool,
) {
    view.on_key(
        wire::keyboard::Event::Press {
            state: wire::keyboard::KeyState {
                key: key.clone(),
                modified_key: key,
                physical_key: wire::keyboard::Physical::Unidentified(
                    wire::keyboard::NativeCode::Unidentified,
                ),
                location: wire::keyboard::Location::Standard,
                modifiers,
            },
            text: None,
            repeat: false,
        },
        captured,
    );
}

/// Press a key, hold it for `repeats` more, then let it go.
fn hold(view: &mut BoardsView, named: wire::keyboard::Named, repeats: usize) {
    let state = || wire::keyboard::KeyState {
        key: wire::keyboard::Key::Named(named),
        modified_key: wire::keyboard::Key::Named(named),
        physical_key: wire::keyboard::Physical::Unidentified(
            wire::keyboard::NativeCode::Unidentified,
        ),
        location: wire::keyboard::Location::Standard,
        modifiers: Default::default(),
    };
    for index in 0..=repeats {
        view.on_key(
            wire::keyboard::Event::Press {
                state: state(),
                text: None,
                repeat: index > 0,
            },
            false,
        );
    }
    view.on_key(wire::keyboard::Event::Release(state()), false);
}

#[test]
fn selection_moves_and_undoes_as_one_gesture_before_receipts() {
    let mut view = view();
    view.camera = [0., 0.];
    view.snap = false;
    card(&mut view, "a", 40);
    card(&mut view, "b", 400);
    view.on_press(10., -10.);
    view.on_move(650., 200.);
    view.on_release();
    assert_eq!(
        view.selected.len(),
        2,
        "marquee includes pending local cards"
    );
    let before = view.pending.len();
    view.on_press(80., 40.);
    view.on_move(110., 60.);
    view.on_release();
    assert_eq!(view.pending.len(), before + 1);
    assert_eq!(view.visible().unwrap().shapes["b"].shape.x, 430);
    view.on_undo();
    assert_eq!(view.visible().unwrap().shapes["a"].shape.x, 40);
    assert_eq!(view.visible().unwrap().shapes["b"].shape.x, 400);
    view.on_redo();
    assert_eq!(view.visible().unwrap().shapes["a"].shape.x, 70);
}

#[test]
fn shortcuts_respect_text_inputs_and_space_is_temporary() {
    use wire::keyboard::{Key, Modifiers, Named};
    let mut view = view();
    key(
        &mut view,
        Key::Character("n".into()),
        Modifiers::default(),
        false,
    );
    assert_eq!(view.tool, Tool::Note);
    key(
        &mut view,
        Key::Character("v".into()),
        Modifiers::default(),
        true,
    );
    assert_eq!(view.tool, Tool::Note, "native input owns captured letters");
    key(
        &mut view,
        Key::Named(Named::Space),
        Modifiers::default(),
        false,
    );
    view.on_press(10., 10.);
    view.on_move(40., 20.);
    assert!(matches!(view.gesture, Gesture::Pan { .. }));
    assert_eq!(view.tool, Tool::Note);
    key(
        &mut view,
        Key::Named(Named::Escape),
        Modifiers::default(),
        false,
    );
    assert!(!view.space_pan);
    assert!(matches!(view.gesture, Gesture::Idle));
    card(&mut view, "a", 0);
    view.selected = ["a".into()].into();
    view.begin_text();
    key(
        &mut view,
        Key::Character("r".into()),
        Modifiers::default(),
        false,
    );
    assert_eq!(
        view.tool,
        Tool::Note,
        "editing also owns uncaptured letters"
    );
}

#[test]
fn remote_delete_does_not_leave_a_crashing_selection_and_editor_drafts_survive_snapshot() {
    let mut view = view();
    view.selected = ["gone".into()].into();
    view.on_press(100., 100.);
    assert!(view.selected.is_empty());
    card(&mut view, "a", 0);
    view.selected = ["a".into()].into();
    view.begin_text();
    view.inline.as_mut().unwrap().document = Editor::new("첫 줄\nSecond line");
    let mut restored = BoardsView::restore(&view.snapshot().unwrap()).unwrap();
    assert_eq!(
        restored.inline.as_ref().unwrap().document.text(),
        "첫 줄\nSecond line"
    );
    restored.finish_text();
    assert_eq!(
        restored.visible().unwrap().shapes["a"].shape.text,
        "첫 줄\nSecond line"
    );
    restored.on_undo();
    assert_eq!(restored.visible().unwrap().shapes["a"].shape.text, "");
}

#[test]
fn editing_clicks_do_not_close_text_and_network_switch_preserves_the_draft() {
    let mut view = view();
    view.camera = [0., 0.];
    card(&mut view, "a", 0);
    view.selected = ["a".into()].into();
    view.begin_text();
    view.on_press(40., 30.);
    assert!(view.inline.is_some());
    view.on_release();
    assert!(view.error.is_empty(), "idle release is not an empty edit");
    view.inline.as_mut().unwrap().document = Editor::new("Keep my draft");
    view.on_session(Ok(host::Session {
        chain: "another-network".into(),
        dark: false,
        connected: true,
    }));
    assert_eq!(
        view.inline.as_ref().unwrap().document.text(),
        "Keep my draft"
    );
    assert!(view.session.chain.is_empty());
}

#[test]
fn help_uses_the_shifted_slash_key_and_prevents_edits_behind_it() {
    let mut view = view();
    use wire::keyboard::{Key, Modifiers, Named};
    card(&mut view, "a", 0);
    view.selected = ["a".into()].into();
    key(
        &mut view,
        Key::Character("/".into()),
        Modifiers {
            shift: true,
            ..Default::default()
        },
        false,
    );
    assert!(view.help);
    key(
        &mut view,
        Key::Named(Named::Delete),
        Modifiers::default(),
        false,
    );
    assert!(view.visible().unwrap().shapes.contains_key("a"));
    key(
        &mut view,
        Key::Named(Named::Escape),
        Modifiers::default(),
        false,
    );
    assert!(!view.help);
}

#[test]
fn an_arrow_drag_binds_the_cards_it_starts_and_ends_on() {
    let mut view = view();
    card(&mut view, "a", 0);
    card(&mut view, "b", 600);
    let between = view
        .drawn_shape(Kind::Arrow, [100., 70.], [700., 70.])
        .unwrap();
    assert_eq!(
        (between.from.as_deref(), between.to.as_deref()),
        (Some("a"), Some("b"))
    );
    assert_eq!(between.points.len(), 2);
    let leaving = view
        .drawn_shape(Kind::Arrow, [100., 70.], [900., 400.])
        .unwrap();
    assert_eq!(
        (leaving.from.as_deref(), leaving.to.clone()),
        (Some("a"), None),
        "an end in open space stands on its own point"
    );
    let inside = view
        .drawn_shape(Kind::Arrow, [20., 20.], [150., 100.])
        .unwrap();
    assert_eq!(
        (inside.from.clone(), inside.to.clone()),
        (None, None),
        "both ends on one card is a free arrow, not a loop the board refuses"
    );
    assert_eq!(
        view.drawn_shape(Kind::Line, [10., 10.], [12., 12.]),
        None,
        "a click with a connector tool draws nothing"
    );
    // every connector a drag produces is one the board accepts
    for shape in [between, leaving, inside] {
        view.visible()
            .unwrap()
            .changed(&Change::Create {
                id: "drawn".into(),
                shape,
            })
            .unwrap();
    }
}
#[test]
fn the_pen_thins_a_run_to_the_boards_budget_and_a_tap_leaves_a_dot() {
    let view = view();
    let wavy: Vec<[f32; 2]> = (0..4000)
        .map(|i| [i as f32 * 0.4, (i as f32 * 0.05).sin() * 60.])
        .collect();
    let stroke = view.sketched_shape(&wavy).unwrap();
    assert_eq!(stroke.kind, Kind::Draw);
    assert!(stroke.points.len() <= boards::MAX_POINTS);
    assert!(stroke.points.len() > 8, "a wavy run keeps its shape");
    assert!(
        stroke.points.iter().flatten().all(|value| *value >= 0),
        "samples are relative to the box that holds them"
    );
    view.visible()
        .unwrap()
        .changed(&Change::Create {
            id: "stroke".into(),
            shape: stroke,
        })
        .unwrap();
    let dot = view.sketched_shape(&[[10., 10.]]).unwrap();
    assert_eq!(dot.points.len(), 2);
    assert_eq!(view.sketched_shape(&[]), None);
}
#[test]
fn a_run_wider_than_a_shape_may_be_is_fitted_whole_rather_than_clipped() {
    let view = view();
    let long: Vec<[f32; 2]> = (0..200)
        .map(|i| [i as f32 * 100., (i % 2) as f32 * 40.])
        .collect();
    let stroke = view.sketched_shape(&long).unwrap();
    assert!(stroke.width <= boards::MAX_SIZE);
    assert_eq!(
        stroke.points.last().unwrap()[0],
        stroke.width,
        "the last sample still lands on the far edge of the box"
    );
}
#[test]
fn shapes_are_chosen_by_their_own_outline_and_strokes_by_their_line() {
    let mut view = view();
    view.edit(Change::Create {
        id: "o".into(),
        shape: Shape {
            kind: Kind::Ellipse,
            width: 200,
            height: 200,
            ..Default::default()
        },
    });
    assert_eq!(view.hit([100., 100.]).as_deref(), Some("o"));
    assert_eq!(view.hit([6., 6.]), None, "a corner is outside the ellipse");
    let mut line = super::tests::view();
    line.edit(Change::Create {
        id: "l".into(),
        shape: segment(Kind::Line),
    });
    let view = line;
    assert_eq!(view.hit([80., 45.]).as_deref(), Some("l"));
    assert_eq!(
        view.hit([150., 10.]),
        None,
        "the box around a diagonal is not the line"
    );
}
#[test]
fn one_eraser_sweep_is_one_undo_step() {
    let mut view = view();
    view.camera = [0., 0.];
    card(&mut view, "a", 0);
    card(&mut view, "b", 300);
    view.on_tool(Tool::Eraser);
    drag(&mut view, &[[100., 70.], [250., 70.], [400., 70.]]);
    assert!(view.visible().unwrap().shapes.is_empty());
    view.on_undo();
    assert_eq!(view.visible().unwrap().shapes.len(), 2);
}
#[test]
fn a_board_of_strokes_stays_inside_the_hosts_geometry_budget() {
    let mut view = view();
    let mut board = view.confirmed.take().unwrap();
    for i in 0..boards::MAX_SHAPES {
        board = board
            .changed(&Change::Create {
                id: format!("stroke-{i}"),
                shape: Shape {
                    kind: Kind::Draw,
                    x: (i as i32 % 16) * 45,
                    y: (i as i32 / 16) * 35,
                    width: 40,
                    height: 30,
                    points: (0..boards::MAX_POINTS)
                        .map(|p| [(p % 40) as i32, (p * 7 % 30) as i32])
                        .collect(),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    view.confirmed = Some(board);
    view.on_select_all();
    view.on_fit();
    let mut driver =
        ducktape_view_guest::Driver::<BoardsView>::from_snapshot(&view.snapshot().unwrap(), false)
            .unwrap();
    let frame = driver.tick(Vec::new());
    let tree = frame.root.unwrap();
    let bytes = ducktape_view_guest::wire::encode(&tree);
    ducktape_view_guest::wire::decode::<wire::Node>(&bytes)
        .expect("the host decodes every piece of geometry the scene drew");
}

fn stacking(view: &BoardsView) -> Vec<String> {
    let board = view.visible().unwrap();
    board
        .ordered()
        .iter()
        .map(|(id, _)| (*id).clone())
        .collect()
}
#[test]
fn a_pasted_connector_binds_to_the_copies_and_not_the_originals() {
    let mut view = view();
    card(&mut view, "a", 0);
    card(&mut view, "b", 400);
    view.edit(Change::Create {
        id: "edge".into(),
        shape: Shape {
            from: Some("a".into()),
            to: Some("b".into()),
            ..segment(Kind::Arrow)
        },
    });
    view.selected = ["a".into(), "edge".into()].into();
    view.on_copy();
    assert_eq!(
        view.clipboard.len(),
        1,
        "an arrow with an end outside the copy has nothing to be copied against"
    );
    view.selected = ["a".into(), "b".into(), "edge".into()].into();
    view.on_copy();
    let taken = view.clipboard.clone();
    assert_eq!(taken.len(), 3);
    view.on_paste();
    // the ids are minted off-thread; answer the way the host would
    view.on_planted(
        0,
        "room".into(),
        taken,
        [40, 40],
        Ok(vec!["a2".into(), "b2".into(), "e2".into()]),
    );
    let board = view.visible().unwrap();
    let copy = &board.shapes["e2"].shape;
    assert_eq!(
        (copy.from.as_deref(), copy.to.as_deref()),
        (Some("a2"), Some("b2"))
    );
    assert_eq!(board.shapes["a2"].shape.x, 40);
    assert_eq!(board.shapes["a"].shape.x, 0, "the original stays put");
    assert_eq!(
        view.selected,
        ["a2".into(), "b2".into(), "e2".into()].into()
    );
}
#[test]
fn alt_drag_leaves_the_original_and_plants_the_copy_where_the_pointer_let_go() {
    let mut view = view();
    view.camera = [0., 0.];
    card(&mut view, "a", 0);
    view.selected = ["a".into()].into();
    view.modifiers.alt = true;
    drag(&mut view, &[[100., 70.], [300., 170.]]);
    assert_eq!(
        view.visible().unwrap().shapes["a"].shape.x,
        0,
        "an alt-drag never commits the move it was previewing"
    );
    view.on_planted(
        0,
        "room".into(),
        vec![("a".into(), Shape::default())],
        [200, 100],
        Ok(vec!["copy".into()]),
    );
    let board = view.visible().unwrap();
    assert_eq!(board.shapes["copy"].shape.x, 200);
    assert_eq!(board.shapes["copy"].shape.y, 100);
    assert_eq!(board.shapes["a"].shape.x, 0);
}
#[test]
fn stacking_moves_a_shape_and_undo_puts_the_whole_stack_back() {
    let mut view = view();
    card(&mut view, "a", 0);
    card(&mut view, "b", 300);
    card(&mut view, "c", 600);
    assert_eq!(stacking(&view), ["a", "b", "c"]);
    view.selected = ["a".into()].into();
    view.on_stack(true);
    assert_eq!(stacking(&view), ["b", "c", "a"]);
    view.on_stack(false);
    assert_eq!(stacking(&view), ["a", "b", "c"]);
    view.on_undo();
    assert_eq!(
        stacking(&view),
        ["b", "c", "a"],
        "undoing a re-stack restores the stack exactly, not approximately"
    );
    view.on_select_all();
    view.on_stack(true);
    assert_eq!(
        stacking(&view),
        ["b", "c", "a"],
        "raising everything moves nothing"
    );
}
#[test]
fn arranging_lines_a_selection_up_and_spreads_it_evenly() {
    let mut lined = view();
    for (id, x) in [("a", 0), ("b", 100), ("c", 900)] {
        card(&mut lined, id, x);
    }
    lined.on_select_all();
    lined.on_arrange(Arrange::Right);
    let board = lined.visible().unwrap();
    for id in ["a", "b", "c"] {
        let s = &board.shapes[id].shape;
        assert_eq!(s.x + s.width, 1100, "every right edge on the far edge");
    }
    let mut spread = view();
    for (id, x) in [("a", 0), ("b", 100), ("c", 800)] {
        card(&mut spread, id, x);
    }
    spread.on_select_all();
    spread.on_arrange(Arrange::SpreadX);
    let board = spread.visible().unwrap();
    // 1000 wide, 600 of it filled: two gaps of 200, the outermost two kept
    assert_eq!(board.shapes["a"].shape.x, 0);
    assert_eq!(board.shapes["b"].shape.x, 400);
    assert_eq!(board.shapes["c"].shape.x, 800);
}

/// An arrow bound a → b, selected, with the camera at rest: the end holding b
/// sits on b's border facing a, at world (300, 70) and so at screen (380, 150).
fn linked() -> BoardsView {
    let mut view = view();
    card(&mut view, "a", 0);
    card(&mut view, "b", 300);
    card(&mut view, "c", 600);
    view.edit(Change::Create {
        id: "edge".into(),
        shape: Shape {
            from: Some("a".into()),
            to: Some("b".into()),
            ..segment(Kind::Arrow)
        },
    });
    view.selected = ["edge".into()].into();
    view
}

#[test]
fn dragging_an_arrows_end_onto_another_card_rebinds_that_end_and_leaves_the_far_one() {
    let mut view = linked();
    drag(&mut view, &[[380., 150.], [600., 150.], [780., 150.]]);
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    assert_eq!(edge.from.as_deref(), Some("a"));
    assert_eq!(edge.to.as_deref(), Some("c"));
}

#[test]
fn an_arrow_bends_by_the_handle_on_its_line_and_straightens_when_you_put_it_back() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    let board = view.visible().unwrap();
    let run = super::interaction::stroke(&board, &board.shapes["edge"].shape);
    let middle = [
        (run[0][0] + run[1][0]) / 2.,
        (run[0][1] + run[1][1]) / 2.,
    ];
    // An arrow with no bend still offers one, on its line. Taking it and
    // pulling puts a bend there — a straight arrow becomes a curved one with
    // no separate verb for it.
    drag(
        &mut view,
        &[middle, [middle[0], middle[1] - 80.], [middle[0], middle[1] - 160.]],
    );
    let board = view.visible().unwrap();
    let bent = &board.shapes["edge"].shape;
    let run = super::interaction::stroke(&board, bent);
    assert_eq!(run.len(), 3, "the arrow did not bend: {run:?}");
    assert!(
        (run[1][1] - (middle[1] - 160.)).abs() < 2.,
        "the bend is not where it was left: {:?}",
        run[1]
    );
    // Its ends are where they were: a bend is a bend, not a re-route.
    assert_eq!(bent.from.as_deref(), Some("a"));
    assert_eq!(bent.to.as_deref(), Some("b"));
    // And putting it back on the line takes it away again, rather than leaving
    // a sample nobody can see.
    drag(
        &mut view,
        &[[middle[0], middle[1] - 160.], middle, middle],
    );
    let board = view.visible().unwrap();
    let run = super::interaction::stroke(&board, &board.shapes["edge"].shape);
    assert_eq!(run.len(), 2, "the bend outlived the curve: {run:?}");
}

#[test]
fn a_bend_dragged_across_a_card_binds_nothing() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    let board = view.visible().unwrap();
    let edge = board.shapes["edge"].shape.clone();
    let run = super::interaction::stroke(&board, &edge);
    let middle = [
        (run[0][0] + run[1][0]) / 2.,
        (run[0][1] + run[1][1]) / 2.,
    ];
    // Card "c" sits at x 600. Drag the bend right over it and let go: the ends
    // are what hold cards, so the arrow must still run a → b.
    let over_c = [650., 60.];
    drag(&mut view, &[middle, [500., 60.], over_c]);
    let board = view.visible().unwrap();
    let bent = &board.shapes["edge"].shape;
    assert_eq!(bent.from.as_deref(), Some("a"));
    assert_eq!(
        bent.to.as_deref(),
        Some("b"),
        "the bend stole the far end's card"
    );
}

#[test]
fn words_on_an_arrow_take_the_middle_and_the_bend_handle_steps_aside() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    let board = view.visible().unwrap();
    let plain = board.shapes["edge"].shape.clone();
    let run = super::interaction::stroke(&board, &plain);
    let middle = [
        (run[0][0] + run[1][0]) / 2.,
        (run[0][1] + run[1][1]) / 2.,
    ];
    let (_, _, bare) = view.bend(&plain, &run).expect("no handle on a bare arrow");
    assert!(
        (bare[0] - middle[0]).abs() < 0.5,
        "a bare arrow bends at its middle, not at {bare:?}"
    );
    // Write on it and the handle moves off the plate: two things to take hold
    // of in one place is one of them unreachable.
    view.edit(Change::Text {
        id: "edge".into(),
        text: "blocks".into(),
    });
    let board = view.visible().unwrap();
    let written = board.shapes["edge"].shape.clone();
    let run = super::interaction::stroke(&board, &written);
    // This arrow is short enough that the words cover the whole of it, so the
    // bend is not offered at all: a handle you cannot take is worse than none.
    let offered = view.bend(&written, &run);
    let clear = offered.is_none_or(|(_, _, at)| {
        !super::interaction::contains(super::interaction::plate(&run), at)
    });
    assert!(clear, "the handle is under the words");
    // And a press on the words is a press on the words, whatever is near it.
    view.selected = ["edge".into()].into();
    view.on_press(middle[0], middle[1]);
    assert!(
        matches!(view.gesture, Gesture::Idle | Gesture::Move { .. }),
        "pressing the label started {:?}",
        view.gesture
    );
}

#[test]
fn zooming_by_the_buttons_keeps_what_is_in_the_middle_of_the_screen() {
    let mut view = view();
    view.on_size(1080., 800.);
    card(&mut view, "a", 0);
    view.on_fit();
    let centre = [view.viewport[0] / 2., view.viewport[1] / 2.];
    let looking_at = view.world(centre);
    // Seven steps in and five back out. Whatever was under the middle of the
    // screen has to still be under the middle of the screen: a zoom that walks
    // off its own subject is a zoom you have to hunt your board back with.
    for _ in 0..7 {
        view.on_zoom(1.25);
    }
    let close = view.world(centre);
    assert!(
        (close[0] - looking_at[0]).abs() < 0.5 && (close[1] - looking_at[1]).abs() < 0.5,
        "zooming in walked from {looking_at:?} to {close:?}"
    );
    for _ in 0..5 {
        view.on_zoom(0.8);
    }
    let back = view.world(centre);
    assert!(
        (back[0] - looking_at[0]).abs() < 0.5 && (back[1] - looking_at[1]).abs() < 0.5,
        "zooming back out walked from {looking_at:?} to {back:?}"
    );
    // And the card is still drawn, however much bigger than the screen it is.
    let board = view.visible().unwrap();
    assert!(
        view.on_screen(&board, &board.shapes["a"].shape).is_some(),
        "a card larger than the viewport was culled as off it"
    );
}

#[test]
fn a_bent_arrows_words_ride_its_curve_and_not_the_box_around_it() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    // Bend the arrow well below its ends, then write on it. The box around a
    // curve has its centre out in the open air — words written there would be
    // words beside the arrow, not on it.
    view.edit(Change::Route {
        id: "edge".into(),
        x: 200,
        y: 70,
        width: 100,
        height: 200,
        points: vec![[0, 0], [50, 200], [100, 0]],
        from: Some("a".into()),
        to: Some("b".into()),
    });
    view.edit(Change::Text {
        id: "edge".into(),
        text: "waits for".into(),
    });
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    let run = super::interaction::stroke(&board, edge);
    let at = super::interaction::plate(&run);
    let middle = [(at[0] + at[2]) / 2., (at[1] + at[3]) / 2.];
    let on_the_run = run
        .windows(2)
        .map(|step| super::interaction::line_distance(middle, step[0], step[1]))
        .fold(f32::INFINITY, f32::min);
    assert!(
        on_the_run < 1.,
        "the words sit {on_the_run} away from the line they belong to"
    );
    // And a press there takes the arrow, because that is where its words are.
    assert_eq!(view.hit(middle).as_deref(), Some("edge"));
    // The pin the painter hangs those words on is the same box, on screen: one
    // answer to "where are the words", read by the painter, the hit test and
    // the editor alike.
    let json = serde_json::to_string(&view.view()).unwrap();
    let pinned = json
        .split("\"boards/pin/edge\"")
        .nth(1)
        .expect("the arrow's words are not pinned");
    let corner = view.screen(at[0], at[1]);
    let reads = |name: &str| -> f32 {
        let tail = pinned.split(&format!("\"{name}\":")).nth(1).unwrap();
        let end = tail.find([',', '}']).unwrap();
        tail[..end].parse().unwrap()
    };
    assert!(
        (reads("x") - corner[0]).abs() < 1. && (reads("y") - corner[1]).abs() < 1.,
        "the words hang at {:?} and the plate is at {corner:?}",
        [reads("x"), reads("y")]
    );
}

#[test]
fn bending_an_arrow_by_hand_leaves_a_run_the_words_can_ride() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    // The whole gesture, the way a hand does it: take the handle on the line
    // and pull. What it leaves has to be a run the painter draws through and
    // the words sit on — the two answers that used to disagree.
    let board = view.visible().unwrap();
    let run = super::interaction::stroke(&board, &board.shapes["edge"].shape);
    let middle = [
        (run[0][0] + run[1][0]) / 2.,
        (run[0][1] + run[1][1]) / 2.,
    ];
    drag(
        &mut view,
        &[middle, [middle[0], middle[1] + 90.], [middle[0], middle[1] + 170.]],
    );
    view.edit(Change::Text {
        id: "edge".into(),
        text: "waits for".into(),
    });
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    let run = super::interaction::stroke(&board, edge);
    assert_eq!(run.len(), 3, "a hand-bent arrow is three samples: {run:?}");
    let at = super::interaction::plate(&run);
    let seat = [(at[0] + at[2]) / 2., (at[1] + at[3]) / 2.];
    assert!(
        (seat[0] - run[1][0]).abs() < 2. && (seat[1] - run[1][1]).abs() < 2.,
        "the words sit at {seat:?} and the bend is at {:?}",
        run[1]
    );
}

#[test]
fn a_pen_stroke_has_no_bend_handle_because_its_run_is_the_drawing() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    view.edit(Change::Create {
        id: "ink".into(),
        shape: Shape {
            kind: Kind::Draw,
            width: 100,
            height: 40,
            points: vec![[0, 0], [50, 40], [100, 0]],
            ..Default::default()
        },
    });
    let board = view.visible().unwrap();
    let ink = &board.shapes["ink"].shape;
    let run = super::interaction::stroke(&board, ink);
    assert!(
        view.bend(ink, &run).is_none(),
        "a pen stroke offered a handle in the middle of the drawing"
    );
}

#[test]
fn an_end_held_over_a_card_is_already_holding_it() {
    let mut view = linked();
    view.on_size(1400., 900.);
    // Take the far end and hold it over card "c" without letting go. What the
    // board shows while the pointer is down used to be a bare line to the
    // pointer with no binding at all: the arrow jumped to the card's edge on
    // release, so the picture you decided from was not the picture you got.
    view.on_press(380., 150.);
    view.on_move(600., 150.);
    view.on_move(780., 150.);
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    assert_eq!(
        edge.to.as_deref(),
        Some("c"),
        "the end in hand was not holding the card it was over"
    );
    let run = super::interaction::stroke(&board, edge);
    let end = run.last().copied().unwrap();
    let c = &board.shapes["c"].shape;
    let edge_of_c = c.x as f32;
    assert!(
        (end[0] - edge_of_c).abs() < 2.,
        "the run ran to the pointer at 780 instead of the card's edge at \
         {edge_of_c}: it landed at {}",
        end[0]
    );
    // and letting go changes nothing, because it was already decided
    view.on_release();
    let board = view.visible().unwrap();
    assert_eq!(board.shapes["edge"].shape.to.as_deref(), Some("c"));
}

#[test]
fn both_ends_of_an_arrow_being_drawn_ring_the_cards_they_would_take() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.selected = Default::default();
    view.camera = [0., 0.];
    view.zoom = 1.;
    view.tool = Tool::Arrow;
    // Drawing from inside one card to inside another binds both ends on
    // release. While the drag is in flight both cards must say so.
    view.on_press(60., 60.);
    view.on_move(200., 100.);
    view.on_move(360., 60.);
    let board = view.visible().unwrap();
    let mut marks = Vec::new();
    view.paint_marks(&board, 3600, &mut marks);
    let a = &board.shapes["a"].shape;
    let b = &board.shapes["b"].shape;
    for (name, card) in [("a", a), ("b", b)] {
        let at = view.screen(card.x as f32, card.y as f32);
        assert!(
            marks.iter().any(|mark| rings(mark, at)),
            "card {name} was about to be bound and said nothing"
        );
    }
    // And it is drawn as the arrow it will become: stopping at the cards'
    // borders rather than running on into them and snapping back on release.
    let drawn_to = marks
        .iter()
        .find_map(|mark| match mark {
            wire::CanvasCommand::Draw {
                shape: wire::CanvasShape::Path(steps),
                ..
            } => steps.last(),
            _ => None,
        })
        .expect("the arrow in flight was not drawn");
    let wire::CanvasSegment::Line(head) = drawn_to else {
        panic!("the run did not end in a line");
    };
    let border = view.screen(b.x as f32, 0.)[0];
    assert!(
        (head[0] - border).abs() < 2.,
        "the arrow ran on to the pointer at 360 instead of stopping at card \
         b's border at {border}: it reached {}",
        head[0]
    );
    view.on_release();
    let board = view.visible().unwrap();
    let drawn = board
        .shapes
        .values()
        .find(|record| record.shape.kind == Kind::Arrow && record.shape.from.is_some())
        .expect("no arrow was drawn");
    assert_eq!(drawn.shape.from.as_deref(), Some("a"));
    assert_eq!(drawn.shape.to.as_deref(), Some("b"));
}

/// Whether a mark is a rectangle standing at this screen point.
fn rings(mark: &wire::CanvasCommand, at: [f32; 2]) -> bool {
    let wire::CanvasCommand::Draw {
        shape: wire::CanvasShape::Rectangle { position, .. },
        ..
    } = mark
    else {
        return false;
    };
    (position[0] - at[0]).abs() < 1. && (position[1] - at[1]).abs() < 1.
}

#[test]
fn dragging_a_bound_end_onto_open_board_frees_it_and_stands_it_on_its_own_point() {
    let mut view = linked();
    drag(&mut view, &[[380., 150.], [500., 400.], [520., 480.]]);
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    assert_eq!(edge.from.as_deref(), Some("a"));
    assert_eq!(edge.to, None);
    // the end it let go of now stands where the pointer left it
    let run = super::interaction::path_points(edge);
    let end = run.last().copied().unwrap();
    assert!((end[0] - 440.).abs() < 1.5, "x landed at {}", end[0]);
    assert!((end[1] - 400.).abs() < 1.5, "y landed at {}", end[1]);
}

#[test]
fn undo_after_a_reroute_puts_the_whole_run_back_in_one_step() {
    let mut view = linked();
    let before = view.visible().unwrap().shapes["edge"].shape.clone();
    drag(&mut view, &[[380., 150.], [600., 150.], [780., 150.]]);
    assert_ne!(view.visible().unwrap().shapes["edge"].shape, before);
    view.on_undo();
    assert_eq!(view.visible().unwrap().shapes["edge"].shape, before);
}

/// The painter and the inline editor lay out one label, so they must read one
/// description of it. Two type sizes drifting apart is what made a label change
/// size and jump corners the moment you started typing; a second literal here
/// is that bug coming back.
#[test]
fn the_painter_and_the_editor_read_one_description_of_a_label() {
    let painting = include_str!("presentation.rs");
    assert_eq!(
        painting.matches("self.lettering(").count(),
        4,
        "the painter asks once, the inline editor once, the gauge that decides \
         how tall the card must be once, and the box the caret lives in once — \
         any of them reading a second description would lay the words out for \
         a card nobody draws"
    );
    assert!(
        !painting.contains("size: Some((14."),
        "the inline editor must not carry a type size of its own"
    );
    // The editor fills the card it is opened over. Asking it to lay out to
    // its own content instead collapses it to its first line on the native
    // side, so five of a note's six lines go missing the moment a caret
    // appears — the exact difference between editing and reading a card that
    // this description exists to close.
    assert!(
        !painting.contains("height: Some(Length::Shrink)"),
        "the inline editor fills its card; a shrunk one shows one line of many"
    );
}

#[test]
fn an_abandoned_text_shape_leaves_nothing_behind_and_an_empty_sticky_stays() {
    let mut view = view();
    view.edit(Change::Create {
        id: "t".into(),
        shape: Shape {
            kind: Kind::Text,
            width: 280,
            height: 96,
            ..Default::default()
        },
    });
    view.selected = ["t".into()].into();
    view.begin_text();
    view.finish_text();
    let board = view.visible().unwrap();
    assert!(
        !board.shapes.contains_key("t"),
        "a text shape with no words is an invisible hit box, not a shape"
    );
    assert!(!view.selected.contains("t"));
    // A sticky with no words is still a sticky: it has a body to show.
    view.edit(Change::Create {
        id: "n".into(),
        shape: Shape {
            kind: Kind::Note,
            width: 220,
            height: 180,
            ..Default::default()
        },
    });
    view.selected = ["n".into()].into();
    view.begin_text();
    view.finish_text();
    assert!(view.visible().unwrap().shapes.contains_key("n"));
}

#[test]
fn a_card_paints_every_word_its_editor_holds_and_the_marks_keep_their_own_room() {
    let mut view = view();
    view.on_size(1400., 900.);
    let mut board = view.confirmed.take().unwrap();
    // A board of ordinary size, each card carrying the most text one may.
    for i in 0..12 {
        board = board
            .changed(&Change::Create {
                id: format!("card-{i}"),
                shape: Shape {
                    x: (i % 4) * 260,
                    y: (i / 4) * 200,
                    text: "가".repeat(boards::MAX_TEXT / 3),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    view.confirmed = Some(board);
    view.selected = (0..12).map(|i| format!("card-{i}")).collect();
    let json = serde_json::to_string(&view.view()).unwrap();
    let painted = json.matches("가").count();
    assert!(
        painted >= 12 * (boards::MAX_TEXT / 3),
        "every card paints the whole of what its editor would hold, not a prefix of it: {painted}"
    );
    // And the selection is still drawn over all of it — the marks are taken
    // out of the frame's budget before the shapes, not left the remainder.
    assert!(json.contains("boards/overlay"));
    let overlay = json.split("boards/overlay").nth(1).unwrap();
    assert!(
        overlay.matches("Rectangle").count() >= 12,
        "a ring for every selected card survives a board full of text"
    );
}

#[test]
fn shift_squares_a_drawn_box_and_a_click_centres_the_default_on_the_pointer() {
    let mut view = view();
    view.on_tool(Tool::Rectangle);
    view.modifiers.shift = true;
    // world (100,100) → (300,180): the short side grows to the long one
    drag(&mut view, &[[180., 180.], [300., 220.], [380., 260.]]);
    let square = view.creation_shape(Kind::Rectangle, [100., 100.], [300., 180.]);
    assert_eq!(square.width, 200);
    assert_eq!(square.height, 200);
    // and it grows away from the corner the press anchored
    assert_eq!([square.x, square.y], [100, 100]);
    view.modifiers.shift = false;
    let clicked = view.creation_shape(Kind::Rectangle, [500., 300.], [500., 300.]);
    assert_eq!([clicked.width, clicked.height], [240, 140]);
    assert_eq!([clicked.x, clicked.y], [380, 230]);
    // Words go the other way: you click where the sentence should start, so
    // the box begins at the pointer rather than straddling it — and it starts
    // one line tall, because a text shape taller than its words is dead space
    // that still answers a click.
    let written = view.creation_shape(Kind::Text, [500., 300.], [500., 300.]);
    assert_eq!([written.x, written.y], [500, 300]);
    assert_eq!(written.height, 60);
}

#[test]
fn a_marquee_takes_a_bound_connector_because_it_is_drawn_between_the_cards_it_names() {
    let mut view = linked();
    view.selected.clear();
    // a band over a and b: the arrow between them is drawn inside it, even
    // though its own samples say otherwise
    drag(&mut view, &[[60., 60.], [300., 150.], [620., 260.]]);
    assert!(view.selected.contains("a"), "the band missed a card");
    assert!(view.selected.contains("b"), "the band missed a card");
    assert!(
        view.selected.contains("edge"),
        "the band missed the connector drawn between them"
    );
}

#[test]
fn the_pen_keeps_itself_and_every_other_tool_hands_back_to_select() {
    let mut view = view();
    view.tool = Tool::Draw;
    view.on_minted(0, "room".into(), segment(Kind::Draw), Ok("mark".into()));
    assert_eq!(
        view.tool,
        Tool::Draw,
        "a second stroke needs no second pick"
    );
    view.tool = Tool::Rectangle;
    view.on_minted(
        0,
        "room".into(),
        Shape {
            x: 400,
            ..Default::default()
        },
        Ok("box".into()),
    );
    assert_eq!(view.tool, Tool::Select);
}

#[test]
fn an_arrow_meets_a_circle_on_its_curve_and_a_diamond_on_its_point() {
    let round = Shape {
        kind: Kind::Ellipse,
        width: 200,
        height: 200,
        ..Default::default()
    };
    // straight out to the right: every outline leaves at the same place
    let east = super::interaction::border_point(&round, [1000., 100.]);
    assert!((east[0] - 200.).abs() < 0.5, "east landed at {}", east[0]);
    // on the diagonal a circle is further in than the box around it
    let corner = super::interaction::border_point(&round, [1000., 1000.]);
    let reach = (corner[0] - 100.).hypot(corner[1] - 100.);
    assert!(
        (reach - 100.).abs() < 0.5,
        "the ray left the curve at {reach}"
    );
    let gem = Shape {
        kind: Kind::Diamond,
        ..round.clone()
    };
    let facet = super::interaction::border_point(&gem, [1000., 1000.]);
    // a diamond's edge runs |dx| + |dy| = half, so the diagonal exit is nearer
    assert!(
        (facet[0] - 150.).abs() < 0.5,
        "facet landed at {}",
        facet[0]
    );
}

#[test]
fn the_pointer_says_what_it_would_take_before_you_press_and_only_when_picking() {
    let mut view = view();
    card(&mut view, "a", 0);
    // over the card with Select armed: the board answers
    view.on_move(180., 150.);
    assert_eq!(view.hover.as_deref(), Some("a"));
    // off it: nothing to say
    view.on_move(900., 700.);
    assert_eq!(view.hover, None);
    // a shape tool is about to draw, so nothing under the pointer is its to
    // offer, however squarely the pointer sits on a card
    view.on_tool(Tool::Rectangle);
    view.on_move(180., 150.);
    assert_eq!(view.hover, None);
    // and a gesture in progress is not a question about what is underneath
    view.on_tool(Tool::Select);
    drag(&mut view, &[[180., 150.], [260., 200.]]);
    view.on_press(180., 150.);
    view.on_move(200., 160.);
    assert_eq!(view.hover, None);
}

#[test]
fn a_handle_on_a_flat_line_stays_where_the_pointer_put_it() {
    let mut view = view();
    view.edit(Change::Create {
        id: "rule".into(),
        shape: Shape {
            width: 300,
            height: 0,
            points: vec![[0, 0], [300, 0]],
            ..segment(Kind::Line)
        },
    });
    view.selected = ["rule".into()].into();
    // drag the far end straight out along the line: a card's 32-unit floor
    // would have thrown it down the moment it moved
    drag(&mut view, &[[380., 80.], [440., 80.], [500., 80.]]);
    let board = view.visible().unwrap();
    let rule = &board.shapes["rule"].shape;
    assert_eq!(rule.height, 0, "the run was forced off the flat");
    assert!(
        rule.width >= 400,
        "the far end did not travel: {}",
        rule.width
    );
}

#[test]
fn an_edge_handle_changes_one_side_and_leaves_the_other_where_it_was() {
    let mut view = view();
    view.snap = false;
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            width: 200,
            height: 120,
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    // the middle of the right edge is world (200,60) = screen (280,140)
    drag(&mut view, &[[280., 140.], [340., 200.], [400., 220.]]);
    let a = view.visible().unwrap().shapes["a"].shape.clone();
    assert_eq!(a.height, 120, "an edge handle moved the other side too");
    assert_eq!([a.x, a.y], [0, 0], "an edge handle moved the far corner");
    assert!(a.width >= 300, "the edge did not travel: {}", a.width);
}

#[test]
fn a_selection_of_several_is_taken_by_the_one_box_drawn_around_it() {
    let mut view = view();
    view.snap = false;
    for (id, x) in [("a", 0), ("b", 300)] {
        view.edit(Change::Create {
            id: id.into(),
            shape: Shape {
                x,
                width: 200,
                height: 100,
                ..Default::default()
            },
        });
    }
    view.selected = ["a".into(), "b".into()].into();
    let board = view.visible().unwrap();
    let (bounds, members) = view.group(&board).expect("a selection of two has a box");
    assert_eq!(
        bounds,
        [0., 0., 500., 100.],
        "the box is the span of the two"
    );
    assert_eq!(members.len(), 2);
    // the middle of that box's right edge is world (500,50) = screen (580,130)
    drag(&mut view, &[[580., 130.], [700., 130.], [830., 130.]]);
    let board = view.visible().unwrap();
    let (a, b) = (
        board.shapes["a"].shape.clone(),
        board.shapes["b"].shape.clone(),
    );
    // the box went from 500 wide to 750, and each member took its share of it
    assert_eq!(a.width, 300);
    assert_eq!([b.x, b.width], [450, 300]);
    assert_eq!(
        [a.height, b.height],
        [100, 100],
        "the other axis was pinned"
    );
    // one shape alone wears its own handles, so no second box is drawn round it
    view.selected = ["a".into()].into();
    let board = view.visible().unwrap();
    assert!(view.group(&board).is_none());
}

#[test]
fn a_held_arrow_key_is_one_edit_however_long_it_is_held() {
    let mut view = view();
    view.snap = false;
    card(&mut view, "a", 0);
    let settled = view.pending.len();
    let undos = view.undo.len();
    view.selected = ["a".into()].into();
    hold(&mut view, wire::keyboard::Named::ArrowRight, 29);
    let board = view.visible().unwrap();
    assert_eq!(
        board.shapes["a"].shape.x, 30,
        "every repeat moved the card by one"
    );
    assert_eq!(
        view.pending.len() - settled,
        1,
        "thirty repeats went to consensus as one edit"
    );
    assert_eq!(
        view.undo.len() - undos,
        1,
        "and come back in one press of undo"
    );
}

#[test]
fn a_nudge_shows_on_the_board_before_it_is_let_go_of() {
    let mut view = view();
    view.snap = false;
    card(&mut view, "a", 0);
    let settled = view.pending.len();
    view.selected = ["a".into()].into();
    key(
        &mut view,
        wire::keyboard::Key::Named(wire::keyboard::Named::ArrowRight),
        Default::default(),
        false,
    );
    assert_eq!(
        view.visible().unwrap().shapes["a"].shape.x,
        1,
        "a held key moves the board the way a held button does"
    );
    assert_eq!(
        view.pending.len(),
        settled,
        "and nothing was sent for it yet"
    );
}

#[test]
fn the_grid_is_a_ruler_the_camera_moves_over_and_not_wallpaper() {
    let mut view = view();
    view.on_size(1200., 800.);
    // The step is read back off the dots: the gap between the first two in a
    // row, divided by the zoom, is how many board units one square is worth.
    let step = |view: &BoardsView| {
        let dots = view.grid(3600);
        let mut xs: Vec<f32> = dots
            .iter()
            .filter_map(|draw| match draw {
                wire::CanvasCommand::Draw {
                    shape: wire::CanvasShape::Circle { center, .. },
                    ..
                } => Some(center[0]),
                _ => None,
            })
            .collect();
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        assert!(xs.len() > 2, "the lattice is too sparse to measure");
        (xs[1] - xs[0]) / view.zoom
    };
    for (zoom, expected) in [(1., 32.), (0.5, 64.), (0.25, 128.), (2., 32.), (8., 32.)] {
        view.zoom = zoom;
        assert_eq!(
            step(&view),
            expected,
            "at {zoom}x a square should be {expected} board units"
        );
    }
    // And whatever the step, the dots stay far enough apart to read as dots.
    for zoom in [0.1, 0.35, 1.7, 6.] {
        view.zoom = zoom;
        assert!(
            step(&view) * zoom >= 24.,
            "the dots ran together at {zoom}x"
        );
    }
}

#[test]
fn the_colour_for_the_next_shape_is_reachable_with_nothing_selected() {
    let mut view = view();
    assert!(view.selected.is_empty());
    let json = serde_json::to_string(&view.view()).unwrap();
    assert!(
        json.contains("boards/colors"),
        "the palette is the only way to choose a colour before drawing"
    );
    // and it is the colour the next shape is actually drawn in
    view.on_color(3);
    assert_eq!(
        view.creation_shape(Kind::Rectangle, [0., 0.], [0., 0.])
            .color,
        3
    );
}

#[test]
fn a_card_too_long_to_save_can_still_be_left() {
    let mut view = view();
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "kept".into(),
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    view.begin_text();
    let overlong = "x".repeat(boards::MAX_TEXT + 10);
    view.inline.as_mut().unwrap().document = Editor::new(overlong);
    // Done keeps the words and says what is wrong, by how much, and the way out
    view.finish_text();
    assert!(view.inline.is_some(), "Done must not lose what you wrote");
    assert!(view.error.contains(&format!("{}", boards::MAX_TEXT + 10)));
    assert!(view.error.contains("Escape"));
    // and Escape is that way out: the card goes back to what it held
    view.on_cancel();
    assert!(view.inline.is_none(), "Escape left the editor open");
    assert!(view.error.is_empty());
    assert_eq!(view.visible().unwrap().shapes["a"].shape.text, "kept");
}

#[test]
fn the_two_keys_that_leave_a_card_are_not_the_same_answer() {
    // The editor claims Escape and Command-Enter, and both arrive as one
    // commit. They must part on the key that asked for it: Command-Enter
    // keeps what you wrote, Escape is the way out of a card the board will
    // not take. Routing both to the same message is what made an over-long
    // card impossible to leave, and it is invisible in every other test —
    // the claim is the host's side of a seam this crate cannot drive.
    let source = include_str!("presentation.rs");
    assert!(
        source.contains("Named::Escape) =>"),
        "the observer must tell Escape apart from the other claimed key"
    );
    assert_eq!(
        source.matches("Some(Message::Cancel)").count(),
        1,
        "Escape leaves through the cancel path, once"
    );
}

#[test]
fn a_card_grows_to_hold_what_you_type_and_keeps_the_height_when_it_is_saved() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "one line".into(),
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    view.begin_text();
    let short = view.visible().unwrap().shapes["a"].shape.height;
    // The host lays the gauge out and says the words come to 420 px. At this
    // zoom that is 420 board units, well past the 140 the card was made at.
    view.inline.as_mut().unwrap().document = Editor::new("a lot more words");
    view.on_measured(200., 420.);
    assert_eq!(
        view.visible().unwrap().shapes["a"].shape.height,
        420,
        "the card under the caret is drawn as tall as its words"
    );
    // A shorter measurement does not take the room back while you are still
    // in the card — the words that need it may come back with the next key.
    view.on_measured(200., 300.);
    assert_eq!(view.visible().unwrap().shapes["a"].shape.height, 420);
    view.finish_text();
    let saved = view.visible().unwrap().shapes["a"].shape.clone();
    assert_eq!(saved.text, "a lot more words");
    assert_eq!(
        saved.height, 420,
        "a card that snapped back on save was clipping the whole time"
    );
    assert!(saved.height > short);
}

#[test]
fn a_card_that_already_holds_its_words_is_left_alone_and_a_refused_one_gives_the_room_back() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "one line".into(),
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    view.begin_text();
    // Words that fit ask for nothing: the card keeps the size it was drawn at
    // and leaving it saves nothing.
    view.on_measured(200., 60.);
    assert_eq!(view.visible().unwrap().shapes["a"].shape.height, 140);
    let before = view.pending.len();
    view.finish_text();
    assert_eq!(view.pending.len(), before, "nothing changed, nothing saved");
    // A card the board will not take gives the room back when you leave it.
    // The height rode the words, so it goes out with them: a card left holding
    // its old sentence in a box sized for one it never kept is a card that
    // remembers a draft nobody saved.
    view.begin_text();
    view.inline.as_mut().unwrap().document = Editor::new("x".repeat(boards::MAX_TEXT + 10));
    view.on_measured(200., 500.);
    assert_eq!(view.visible().unwrap().shapes["a"].shape.height, 500);
    view.finish_text();
    assert!(view.inline.is_some(), "the board refuses text this long");
    view.on_cancel();
    let left = view.visible().unwrap().shapes["a"].shape.clone();
    assert_eq!(left.height, 140, "Escape leaves the card as it was");
    assert_eq!(left.text, "one line");
}

#[test]
fn an_arrow_held_at_one_end_still_moves_the_end_it_owns() {
    let mut view = linked();
    view.snap = false;
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    // Free the far end and leave the near one held. The arrow now owns one
    // point and a card owns the other, and a drag used to move neither: every
    // bound connector was refused, so this was an arrow you could select, see
    // selected, and not move.
    view.edit(Change::Route {
        id: "edge".into(),
        x: 200,
        y: 70,
        width: 160,
        height: 90,
        points: vec![[0, 0], [160, 90]],
        from: Some("a".into()),
        to: None,
    });
    let board = view.visible().unwrap();
    let before = super::interaction::stroke(&board, &board.shapes["edge"].shape);
    let loose = *before.last().unwrap();
    // A quarter of the way along: the middle of a connector is its bend
    // handle, and the rest of the line is what moves it.
    let on_the_line = [
        before[0][0] + (loose[0] - before[0][0]) * 0.25,
        before[0][1] + (loose[1] - before[0][1]) * 0.25,
    ];
    view.selected = ["edge".into()].into();
    drag(
        &mut view,
        &[
            on_the_line,
            [on_the_line[0] + 40., on_the_line[1] + 40.],
            [on_the_line[0] + 60., on_the_line[1] + 60.],
        ],
    );
    let board = view.visible().unwrap();
    let edge = &board.shapes["edge"].shape;
    assert_eq!(edge.from.as_deref(), Some("a"), "the drag broke the binding");
    let after = super::interaction::stroke(&board, edge);
    let moved = *after.last().unwrap();
    assert!(
        (moved[0] - (loose[0] + 60.)).abs() < 2. && (moved[1] - (loose[1] + 60.)).abs() < 2.,
        "the free end went from {loose:?} to {moved:?} on a 60-unit drag"
    );
    // The held end is still the card's business, not the drag's.
    let card = &board.shapes["a"].shape;
    assert!(
        super::interaction::contains(super::interaction::rect(card), after[0])
            || (after[0][0] - (card.x + card.width) as f32).abs() < 2.,
        "the held end left its card at {:?}",
        after[0]
    );
}

#[test]
fn a_selection_box_holds_the_half_held_arrow_it_carries() {
    let mut view = view();
    view.snap = false;
    card(&mut view, "a", 0);
    view.edit(Change::Create {
        id: "edge".into(),
        shape: Shape {
            x: 200,
            y: 400,
            from: Some("a".into()),
            to: None,
            ..segment(Kind::Arrow)
        },
    });
    view.selected = ["a".into(), "edge".into()].into();
    let board = view.visible().unwrap();
    let (bounds, members) = view.group(&board).expect("two selected have a box");
    assert_eq!(
        members.len(),
        2,
        "the arrow owns an end, so the box moves it and must hold it"
    );
    // The box has to reach the end the arrow owns — which is down at y 490,
    // not up where a card ends.
    let run = super::interaction::stroke(&board, &board.shapes["edge"].shape);
    let low = run.iter().fold(f32::MIN, |a, p| a.max(p[1]));
    assert!(
        bounds[1] + bounds[3] >= low - 1.,
        "the box stops at {} and the arrow reaches {low}",
        bounds[1] + bounds[3]
    );
}

#[test]
fn the_box_drawn_round_a_selection_is_the_box_round_what_it_moves() {
    let mut view = view();
    view.snap = false;
    card(&mut view, "a", 0);
    card(&mut view, "b", 300);
    // An arrow that holds both cards. Its stored rectangle is wherever it was
    // drawn — far above the cards here — and it is never restated when a card
    // it names moves, because the run is recomputed from the cards each frame.
    view.edit(Change::Create {
        id: "edge".into(),
        shape: Shape {
            x: -400,
            y: -900,
            from: Some("a".into()),
            to: Some("b".into()),
            ..segment(Kind::Arrow)
        },
    });
    view.selected = ["a".into(), "b".into(), "edge".into()].into();
    let board = view.visible().unwrap();
    let (bounds, members) = view.group(&board).expect("three selected have a box");
    assert_eq!(
        members.len(),
        2,
        "a connector held by its cards is carried, not moved"
    );
    let cards = [
        board.shapes["a"].shape.clone(),
        board.shapes["b"].shape.clone(),
    ];
    let top = cards[0].y.min(cards[1].y) as f32;
    let left = cards[0].x.min(cards[1].x) as f32;
    assert_eq!(
        [bounds[0], bounds[1]],
        [left, top],
        "the box stood in a corner none of the cards reach"
    );
}

/// The only thing that ever pointed the keyboard at the canvas was leaving a
/// card, so on a board nobody had written in yet no shortcut worked at all: N
/// made no note, Delete deleted nothing, and the fix for it was to go and edit
/// something first. Every way of arriving at a board hands the keyboard over
/// now. A unit test cannot drive the host's focus, so the seam is pinned here.
#[test]
fn every_way_of_arriving_at_a_board_points_the_keyboard_at_it() {
    let mut view = view();
    // the stage appearing is also a measurement
    view.on_mounted(1400., 900.);
    assert_eq!(view.viewport, [1400., 900.]);
    let acting = include_str!("interaction.rs");
    let opening = include_str!("lib.rs");
    assert_eq!(
        acting.matches("self.take_the_keyboard()").count(),
        3,
        "the stage appearing, the menu closing, and Escape"
    );
    assert_eq!(
        opening.matches("self.take_the_keyboard()").count(),
        2,
        "opening a board, and making one"
    );
}

#[test]
fn an_arrow_can_be_written_on_and_the_words_ride_the_run_rather_than_its_stored_box() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.selected = ["edge".into()].into();
    // A connector could carry words — the painter drew them — and there was no
    // way to type any: the one door into the editor turned a connector away.
    view.begin_text();
    assert!(view.inline.is_some(), "a connector would not open");
    view.inline.as_mut().unwrap().document = Editor::new("holds");
    view.finish_text();
    let board = view.visible().unwrap();
    assert_eq!(board.shapes["edge"].shape.text, "holds");
    // Its box is the span of its run and nothing chose it, so writing on it
    // must not resize it the way writing in a card does.
    let edge = board.shapes["edge"].shape.clone();
    assert_eq!(
        [edge.width, edge.height],
        [160, 90],
        "a connector's geometry is its samples, not a box that grows"
    );
    // And the words ride the middle of the run, which is where the arrow is
    // drawn — not the rectangle its samples were stored with.
    let json = serde_json::to_string(&view.view()).unwrap();
    assert!(json.contains("holds"));
    // The plate is part of the connector: a press on the words takes it, the
    // way a press on a card's words takes the card.
    let board = view.visible().unwrap();
    let run = super::interaction::stroke(&board, &edge);
    let middle = [
        (run[0][0] + run[run.len() - 1][0]) / 2.,
        (run[0][1] + run[run.len() - 1][1]) / 2.,
    ];
    let beside = [middle[0], middle[1] - 20.];
    assert_eq!(
        view.hit(beside).as_deref(),
        Some("edge"),
        "the words were readable and unreachable"
    );
}

#[test]
fn an_arrows_words_ride_a_plate_that_hugs_them_and_a_cards_fill_its_box() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.selected = Default::default();
    view.edit(Change::Text {
        id: "edge".into(),
        text: "depends on".into(),
    });
    view.edit(Change::Text {
        id: "a".into(),
        text: "a thought".into(),
    });
    let json = serde_json::to_string(&view.view()).unwrap();
    // The room a label is given is the longest one a line could carry. A plate
    // filling that room rubs the line out either side of two short words, so
    // the plate hugs the words and is centred in the room instead — which is
    // also what puts them on the line rather than off to its left.
    let plate = json
        .split("boards/plate/edge")
        .nth(1)
        .expect("the arrow's words are not on a plate");
    let plate = &plate[..plate.len().min(400)];
    assert!(plate.contains("Shrink"), "the plate fills the room: {plate}");
    assert!(plate.contains("Center"), "the plate is not centred: {plate}");
    // A card is a page: its words start at its own corner and fill it.
    let card = json
        .split("boards/label-clip/a")
        .nth(1)
        .expect("the card's words are not in its box");
    let card = &card[..card.len().min(400)];
    assert!(
        !card.contains("Shrink"),
        "a card's words stopped filling it: {card}"
    );
}

#[test]
fn coming_back_to_a_card_puts_the_caret_after_the_words_it_already_holds() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "first\nsecond".into(),
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    view.begin_text();
    let cursor = view.inline.as_ref().unwrap().document.cursor();
    // Typing on a card you have written on adds to it. A caret left in front
    // of the first letter put every new word ahead of every kept one.
    assert_eq!(cursor.position.line, 1, "the caret is not on the last line");
    assert_eq!(
        cursor.position.column, 6,
        "the caret is not after the last word"
    );
    assert_eq!(cursor.selection, None, "the caret arrived holding a selection");
}

#[test]
fn the_caret_hugs_an_arrows_words_and_takes_a_whole_card() {
    let mut view = linked();
    view.on_size(1400., 900.);
    // Nothing measured yet: the caret takes the room, because a plate hugging
    // words nobody has measured would be a plate of no width at all.
    view.selected = ["edge".into()].into();
    view.begin_text();
    let board = view.visible().unwrap();
    let edge = board.shapes["edge"].shape.clone();
    let run = super::interaction::stroke(&board, &edge);
    let box_ = super::interaction::plate(&run);
    let (pos, room) = view.writing_box(&board, &edge, box_);
    let inline = view.inline.clone().unwrap();
    assert_eq!(view.caret_box(&inline, edge.kind, pos, room), (pos, room));
    // Once the gauge has answered, the caret is as wide as the words and sits
    // at the middle of the room — the same middle the painter centres the
    // saved plate on, so the words do not jump when you stop typing.
    view.on_measured(60., 24.);
    let inline = view.inline.clone().unwrap();
    let (caret, size) = view.caret_box(&inline, edge.kind, pos, room);
    assert!(size[0] < room[0], "the caret still took the whole room");
    assert!(
        (caret[0] + size[0] / 2. - (pos[0] + room[0] / 2.)).abs() < 0.5,
        "the caret is not centred where the plate will be"
    );
    // A card is written across the whole card whatever the gauge says.
    let card = board.shapes["a"].shape.clone();
    let (pos, room) = view.writing_box(&board, &card, [0., 0., 200., 120.]);
    assert_eq!(view.caret_box(&inline, Kind::Note, pos, room), (pos, room));
}

#[test]
fn a_press_in_an_arrows_label_keeps_the_editor_open_the_way_a_press_in_a_cards_does() {
    let mut view = linked();
    view.on_size(1400., 900.);
    view.camera = [0., 0.];
    view.zoom = 1.;
    view.selected = ["edge".into()].into();
    view.begin_text();
    assert!(view.inline.is_some(), "a connector would not open");
    let board = view.visible().unwrap();
    let edge = board.shapes["edge"].shape.clone();
    let run = super::interaction::stroke(&board, &edge);
    let middle = [
        (run[0][0] + run[run.len() - 1][0]) / 2.,
        (run[0][1] + run[run.len() - 1][1]) / 2.,
    ];
    // The host delivers a press around every double-click. For a card it lands
    // inside the card's own box and is read as a press in the words you are
    // typing. A bound connector's stored box is wherever it was first drawn —
    // the ends have followed their cards since — so that press missed, counted
    // as a press on the board, and shut the editor the instant it opened.
    assert!(
        !super::interaction::contains(super::interaction::rect(&edge), middle),
        "this arrow's stored box still covers its run, so it proves nothing"
    );
    view.on_press(middle[0], middle[1]);
    assert!(
        view.inline.is_some(),
        "the press on the words being typed shut the editor"
    );
    // Off the plate it is a press on the board again: the label saves and the
    // editor closes, exactly as it does for a card.
    view.inline.as_mut().unwrap().document = Editor::new("holds");
    view.on_press(middle[0], middle[1] + 400.);
    assert!(
        view.inline.is_none(),
        "a press out on the open board left the editor standing"
    );
    assert_eq!(view.visible().unwrap().shapes["edge"].shape.text, "holds");
}

#[test]
fn a_card_too_small_to_read_is_drawn_without_its_words() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "a sentence that would not fit".into(),
            ..Default::default()
        },
    });
    let close = serde_json::to_string(&view.view()).unwrap();
    assert!(close.contains("a sentence that would not fit"));
    // Far enough out the type stops shrinking with the card — it has a floor
    // and the card does not — so the card fills with a fragment of its first
    // line. At that distance the board is blocks of colour and nothing else.
    view.zoom = 0.2;
    let far = serde_json::to_string(&view.view()).unwrap();
    assert!(
        !far.contains("a sentence that would not fit"),
        "a card two hundred pixels away was still trying to spell"
    );
    // and the card itself is still drawn
    assert!(far.contains("Canvas"));
}

#[test]
fn a_board_is_named_with_the_letters_the_tools_answer_to() {
    let mut view = view();
    view.on_board_picker();
    assert!(view.picking_a_board());
    // "Note" typed into the name field: o is the ellipse, t is the text tool,
    // e is the eraser. Every one of them used to land on the canvas behind.
    for letter in ["n", "o", "t", "e"] {
        key(
            &mut view,
            wire::keyboard::Key::Character(letter.into()),
            Default::default(),
            false,
        );
    }
    assert_eq!(
        view.tool,
        Tool::Select,
        "the menu is what you are typing at, not the canvas"
    );
    // Escape is the one key that reaches past it, and it closes the menu.
    key(
        &mut view,
        wire::keyboard::Key::Named(wire::keyboard::Named::Escape),
        Default::default(),
        false,
    );
    assert!(!view.picking_a_board());
    // With the menu gone the canvas answers again.
    key(
        &mut view,
        wire::keyboard::Key::Character("o".into()),
        Default::default(),
        false,
    );
    assert_eq!(view.tool, Tool::Ellipse);
}

/// The gauge is the only way a card can know how tall its words are: the
/// native editor shows one line when asked to size itself, and the wire has no
/// verb for measuring a document. It must carry what the editor holds — not
/// what the board holds — or the card is sized for the text you started with.
#[test]
fn the_gauge_carries_the_words_being_typed_and_not_the_ones_already_saved() {
    let mut view = view();
    view.on_size(1400., 900.);
    view.edit(Change::Create {
        id: "a".into(),
        shape: Shape {
            text: "saved words".into(),
            ..Default::default()
        },
    });
    view.selected = ["a".into()].into();
    view.begin_text();
    view.inline.as_mut().unwrap().document = Editor::new("typed words");
    let json = serde_json::to_string(&view.view()).unwrap();
    let gauge = json.split("boards/gauge-pin").nth(1).unwrap();
    let gauge = gauge.split("boards/editor-pin").next().unwrap();
    assert!(
        gauge.contains("typed words"),
        "the gauge measures the document, not the board"
    );
    // And it is invisible: a second copy of the words over the card would
    // double every glyph you typed.
    assert!(
        gauge.contains("\"color\":["),
        "the gauge names a colour of its own"
    );
    let ink = gauge.split("\"color\":[").nth(1).unwrap();
    let ink = ink.split(']').next().unwrap();
    assert!(
        ink.ends_with("0.0"),
        "the gauge is drawn in nothing, it read {ink}"
    );
}
