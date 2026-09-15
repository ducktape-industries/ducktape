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
/// A two-point path. Every connector carries its own samples; binding an
/// endpoint to a card only overrides where that end is drawn.
fn path(kind: Kind) -> Shape {
    Shape {
        kind,
        width: 160,
        height: 90,
        points: vec![[0, 0], [160, 90]],
        ..Default::default()
    }
}

fn stack(board: &Board) -> Vec<&str> {
    board.ordered().iter().map(|(id, _)| id.as_str()).collect()
}

#[test]
fn stacking_names_what_rises_and_naming_everything_states_the_whole_stack() {
    let board = blank()
        .changed_many(&[create("a"), create("b"), create("c")])
        .unwrap();
    assert_eq!(stack(&board), ["a", "b", "c"]);
    let raised = board
        .changed(&Change::Order {
            ids: vec!["a".into()],
        })
        .unwrap();
    assert_eq!(stack(&raised), ["b", "c", "a"]);
    // to send "a" back is to raise everything else, in its own order
    let sunk = raised
        .changed(&Change::Order {
            ids: vec!["b".into(), "c".into()],
        })
        .unwrap();
    assert_eq!(stack(&sunk), ["a", "b", "c"]);
    // naming the whole board restores an exact stack, which is how undo works
    let restored = sunk
        .changed(&Change::Order {
            ids: vec!["b".into(), "c".into(), "a".into()],
        })
        .unwrap();
    assert_eq!(stack(&restored), stack(&raised));
    for ids in [
        vec!["a".into(), "a".into()],
        vec!["missing".into()],
        vec!["a".into(); MAX_SHAPES + 1],
    ] {
        assert!(board.changed(&Change::Order { ids }).is_err());
    }
    // a new shape still lands on top of a renumbered board
    let after = raised.changed(&create("d")).unwrap();
    assert_eq!(stack(&after).last(), Some(&"d"));
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
        from: Some("a".into()),
        to: Some("b".into()),
        ..path(Kind::Arrow)
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
            to: Some("missing".into()),
            ..path(Kind::Arrow)
        },
        // a connector with no samples has nowhere to be drawn
        Shape {
            points: Vec::new(),
            ..path(Kind::Arrow)
        },
        Shape {
            points: vec![[0, 0]; MAX_POINTS + 1],
            ..path(Kind::Draw)
        },
        // only arrows bind; a plain line and a card carry neither endpoint
        Shape {
            to: Some("a".into()),
            ..path(Kind::Line)
        },
        Shape {
            points: vec![[0, 0], [40, 40]],
            ..Default::default()
        },
        // a card keeps a minimum box; a path's box is its samples' span
        Shape {
            height: 8,
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
fn a_flat_stroke_is_legal_and_a_shape_never_changes_family() {
    let mut board = blank().changed(&create("card")).unwrap();
    board = board
        .changed(&Change::Create {
            id: "flat".into(),
            shape: Shape {
                height: 0,
                points: vec![[0, 0], [160, 0]],
                ..path(Kind::Line)
            },
        })
        .unwrap();
    assert_eq!(board.shapes["flat"].shape.height, 0);
    for (id, shape) in [("flat", Shape::default()), ("card", path(Kind::Draw))] {
        assert!(
            board
                .changed(&Change::Create {
                    id: id.into(),
                    shape
                })
                .unwrap()
                == board,
            "create over an existing id is idempotent, never a family swap"
        );
    }
    // resizing a stroke scales its samples; the module only moves the box
    board = board
        .changed(&Change::Resize {
            id: "flat".into(),
            width: 320,
            height: 0,
        })
        .unwrap();
    assert_eq!(board.shapes["flat"].shape.points, vec![[0, 0], [160, 0]]);
}
#[test]
fn an_arrow_binds_one_end_and_stands_on_its_own_point_at_the_other() {
    let board = blank()
        .changed(&create("card"))
        .unwrap()
        .changed(&Change::Create {
            id: "half".into(),
            shape: Shape {
                from: Some("card".into()),
                ..path(Kind::Arrow)
            },
        })
        .unwrap();
    assert_eq!(board.shapes["half"].shape.to, None);
    assert!(
        board
            .changed(&Change::Create {
                id: "loop".into(),
                shape: Shape {
                    from: Some("card".into()),
                    to: Some("card".into()),
                    ..path(Kind::Arrow)
                },
            })
            .is_err()
    );
    // the card goes, and with it every connector that named it
    let after = board
        .changed(&Change::Delete { id: "card".into() })
        .unwrap();
    assert!(after.shapes.is_empty());
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

#[test]
fn the_consensus_reducer_has_one_exhaustive_delegating_dispatch() {
    let file = syn::parse_file(include_str!("../src/interface.rs")).unwrap();
    let reducer = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(item) => Some(item),
            _ => None,
        })
        .flat_map(|item| &item.items)
        .find_map(|item| match item {
            syn::ImplItem::Fn(method) if method.sig.ident == "apply" => Some(method),
            _ => None,
        })
        .unwrap();
    let [syn::Stmt::Expr(syn::Expr::Match(dispatch), None)] = reducer.block.stmts.as_slice() else {
        panic!("one dispatch only")
    };
    for arm in &dispatch.arms {
        assert!(arm.guard.is_none());
        assert!(!matches!(arm.pat, syn::Pat::Wild(_)));
        assert!(
            matches!(*arm.body, syn::Expr::MethodCall(_)),
            "each event delegates to a named pure handler"
        );
    }
}

#[test]
fn batch_is_atomic_and_editing_does_not_change_stacking_order() {
    let original = Board::new("Board".into(), "owner".into()).unwrap();
    let board = original
        .changed_many(&[
            Change::Create {
                id: "z".into(),
                shape: Shape::default(),
            },
            Change::Create {
                id: "a".into(),
                shape: Shape::default(),
            },
        ])
        .unwrap();
    let edited = board
        .changed(&Change::Text {
            id: "z".into(),
            text: "Edited".into(),
        })
        .unwrap();
    assert_eq!(
        edited
            .ordered()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    let failed = board.changed_many(&[
        Change::Move {
            id: "z".into(),
            x: 70,
            y: 10,
        },
        Change::Text {
            id: "a".into(),
            text: "x".repeat(boards::MAX_TEXT + 1),
        },
    ]);
    assert!(failed.is_err());
    assert_eq!(board.shapes["z"].shape.x, 0);
    assert!(original.shapes.is_empty());
}
