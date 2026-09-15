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
        2,
        "the painter asks once and the inline editor asks once"
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
