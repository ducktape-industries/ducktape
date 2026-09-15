use boards::*;
use sdk::{Module, Msg, Origin};
use sdk_testkit::{MemStore, TestCtx};
fn blank() -> Board {
    Board::new("Design room".into(), "alice".into()).unwrap()
}
fn create(id: &str) -> Change {
    Change::Create {
        id: id.into(),
        shape: Shape::default(),
    }
}

#[test]
fn concurrent_fields_compose_and_same_field_follows_consensus_order() {
    let initial = blank().changed(&create("a")).unwrap();
    let edits = [
        Change::Move {
            id: "a".into(),
            x: 50,
            y: 80,
        },
        Change::Text {
            id: "a".into(),
            text: "한글 아이디어 🦆".into(),
        },
    ];
    let a = edits
        .iter()
        .try_fold(initial.clone(), |b, c| b.changed(c))
        .unwrap();
    let b = edits
        .iter()
        .rev()
        .try_fold(initial, |b, c| b.changed(c))
        .unwrap();
    assert_eq!(a.shapes["a"].shape, b.shapes["a"].shape);
    assert_eq!(a.shapes["a"].shape.text, "한글 아이디어 🦆");
    assert_eq!(
        a.changed(&Change::Move {
            id: "a".into(),
            x: 200,
            y: 300
        })
        .unwrap()
        .shapes["a"]
            .shape
            .x,
        200
    );
}
#[test]
fn deleting_a_card_removes_connections_and_late_edits_do_not_resurrect_it() {
    let mut board = blank()
        .changed(&create("a"))
        .unwrap()
        .changed(&create("b"))
        .unwrap();
    let arrow = Shape {
        kind: Kind::Arrow,
        from: Some("a".into()),
        to: Some("b".into()),
        ..Default::default()
    };
    board = board
        .changed(&Change::Create {
            id: "arrow".into(),
            shape: arrow,
        })
        .unwrap();
    board = board.changed(&Change::Delete { id: "a".into() }).unwrap();
    assert_eq!(board.shapes.len(), 1);
    assert_eq!(
        board
            .changed(&Change::Text {
                id: "a".into(),
                text: "late".into()
            })
            .unwrap(),
        board
    );
}
#[test]
fn invalid_geometry_content_and_edges_leave_state_untouched() {
    let board = blank().changed(&create("a")).unwrap();
    for shape in [
        Shape {
            x: i32::MIN,
            ..Default::default()
        },
        Shape {
            text: "x".repeat(MAX_TEXT + 1),
            ..Default::default()
        },
        Shape {
            color: 5,
            ..Default::default()
        },
        Shape {
            kind: Kind::Arrow,
            from: Some("a".into()),
            to: Some("missing".into()),
            ..Default::default()
        },
    ] {
        assert!(
            board
                .changed(&Change::Create {
                    id: "bad".into(),
                    shape
                })
                .is_err()
        );
        assert_eq!(board.shapes.len(), 1);
    }
    let encoded = serde_json::to_vec(&board).unwrap();
    assert_eq!(serde_json::from_slice::<Board>(&encoded).unwrap(), board);
}
#[test]
fn board_caps_bound_storage_and_create_replay_is_idempotent() {
    let mut board = blank();
    for i in 0..MAX_SHAPES {
        board = board.changed(&create(&format!("shape-{i}"))).unwrap();
    }
    assert!(board.changed(&create("overflow")).is_err());
    assert_eq!(board.changed(&create("shape-0")).unwrap(), board);
    assert!(serde_json::to_vec(&board).unwrap().len() < MAX_BOARD_BYTES);
}
#[test]
fn real_module_stages_commits_aborts_and_rejects_unauthenticated_writes() {
    futures::executor::block_on(async {
        let mut module = Boards::new(Box::new(MemStore::new()));
        let mut env = TestCtx::at_height(1).env().clone();
        env.origin = Origin::External(vec![7; 32]);
        let mut ctx = TestCtx::with_env(env);
        let op = |operation| Msg {
            target: "boards".into(),
            payload: serde_json::to_vec(&operation).unwrap(),
        };
        let initial = module.root();
        module
            .execute(
                &mut ctx,
                &op(Operation::Create {
                    id: "room".into(),
                    title: "Planning".into(),
                }),
            )
            .await
            .unwrap();
        module
            .execute(
                &mut ctx,
                &op(Operation::Edit {
                    board: "room".into(),
                    change: create("a"),
                }),
            )
            .await
            .unwrap();
        assert_eq!(module.root(), initial);
        module.commit_block().await.unwrap();
        let committed = module.root();
        assert_ne!(committed, initial);
        module
            .execute(
                &mut ctx,
                &op(Operation::Edit {
                    board: "room".into(),
                    change: Change::Delete { id: "a".into() },
                }),
            )
            .await
            .unwrap();
        module.abort_block().await.unwrap();
        assert_eq!(module.root(), committed);
        let reply = module
            .query(&serde_json::to_vec(&Query::Get { id: "room".into() }).unwrap())
            .await
            .unwrap();
        let Reply::Board(Some(board)) = serde_json::from_slice(&reply).unwrap() else {
            panic!("board reply")
        };
        assert!(board.shapes.contains_key("a"));
        let mut anonymous = TestCtx::at_height(2);
        assert!(
            module
                .execute(
                    &mut anonymous,
                    &op(Operation::Create {
                        id: "bad".into(),
                        title: "No".into()
                    })
                )
                .await
                .is_err()
        );
        assert_eq!(module.root(), committed);
    });
}
use sdk::Ctx;

#[test]
fn board_creator_uses_the_shared_actor_convention_for_passkeys() {
    futures::executor::block_on(async {
        let mut module = Boards::new(Box::new(MemStore::new()));
        let mut env = TestCtx::at_height(1).env().clone();
        env.origin = Origin::External(vec![2; 33]);
        let owner = env.origin.actor_string();
        let mut ctx = TestCtx::with_env(env);
        let op = Operation::Create {
            id: "passkey".into(),
            title: "Passkey board".into(),
        };
        let message = Msg {
            target: "boards".into(),
            payload: serde_json::to_vec(&op).unwrap(),
        };
        module.execute(&mut ctx, &message).await.unwrap();
        module.execute(&mut ctx, &message).await.unwrap();
        let bytes = module
            .query(
                &serde_json::to_vec(&Query::Get {
                    id: "passkey".into(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let Reply::Board(Some(board)) = serde_json::from_slice(&bytes).unwrap() else {
            panic!("board reply")
        };
        assert_eq!(board.owner, owner);
    });
}
