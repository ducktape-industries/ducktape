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
    view.selected = Some("a".into());
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
    view.selected = Some("card-0".into());
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
    assert!(json.contains("boards/text"));
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
