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
            kind: Kind::Arrow,
            from: Some("a".into()),
            to: Some("b".into()),
            ..Default::default()
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
