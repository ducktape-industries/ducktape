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
