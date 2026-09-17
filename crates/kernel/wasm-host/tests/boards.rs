//! Exercise the deployable board bytes through the real Wasmtime/host store seam.
use sdk::{Ctx, Module, Msg, Origin};
use sdk_testkit::{MemStore, TestCtx};
use wasm_host::WasmModule;

const BOARDS: &[u8] = include_bytes!("../../../modules/apps/boards/component.wasm");

#[tokio::test]
async fn boards_component_commits_collaborative_fields_and_rolls_back_rejections() {
    let mut module = WasmModule::with_store("boards", BOARDS, Box::new(MemStore::new())).unwrap();
    let mut replica = WasmModule::with_store("boards", BOARDS, Box::new(MemStore::new())).unwrap();
    let mut env = TestCtx::at_height(1).env().clone();
    env.origin = Origin::External(vec![8; 32]);
    env.me = "boards".into();
    let mut ctx = TestCtx::with_env(env);
    let shape = serde_json::json!({"kind":"note","x":0,"y":0,"width":200,"height":140,"text":"","color":0,"align":"middle","text_size":"medium","points":[],"from":null,"to":null});
    let operations = [
        serde_json::json!({"create":{"id":"room","title":"Planning"}}),
        serde_json::json!({"edit":{"board":"room","change":{"create":{"id":"a","shape":shape}}}}),
        serde_json::json!({"batch":{"board":"room","changes":[
            {"move":{"id":"a","x":100,"y":50}},
            {"text":{"id":"a","text":"같이 생각하기"}}
        ]}}),
    ];
    for operation in operations {
        let msg = Msg {
            target: "boards".into(),
            payload: serde_json::to_vec(&operation).unwrap(),
        };
        module.execute(&mut ctx, &msg).await.unwrap();
        replica.execute(&mut ctx, &msg).await.unwrap();
    }
    module.commit_block().await.unwrap();
    replica.commit_block().await.unwrap();
    assert_eq!(module.root(), replica.root());
    let root = module.root();
    let query = br#"{"get":{"id":"room"}}"#;
    let before = module.query(query).await.unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(
        reply["board"]["shapes"]["a"]["shape"]["text"],
        "같이 생각하기"
    );
    assert_eq!(reply["board"]["shapes"]["a"]["shape"]["x"], 100);
    let invalid = Msg {
        target: "boards".into(),
        payload:
            br#"{"edit":{"board":"room","change":{"resize":{"id":"a","width":-1,"height":40}}}}"#
                .to_vec(),
    };
    assert!(module.execute(&mut ctx, &invalid).await.is_err());
    module.commit_block().await.unwrap();
    assert_eq!(module.root(), root);
    assert_eq!(module.query(query).await.unwrap(), before);
    let delete = Msg {
        target: "boards".into(),
        payload: br#"{"edit":{"board":"room","change":{"delete":{"id":"a"}}}}"#.to_vec(),
    };
    module.execute(&mut ctx, &delete).await.unwrap();
    module.abort_block().await.unwrap();
    assert_eq!(module.query(query).await.unwrap(), before);
}

/// The board-level operations through the same seam: a name is a thing anyone
/// on the network may change, and a board only leaves while nobody has drawn on
/// it. Worth its own trip through the deployable bytes because these are the
/// only two operations that address a board rather than a shape on one, and the
/// removal is the only one that takes state away.
#[tokio::test]
async fn boards_component_renames_for_anyone_and_removes_only_an_empty_board() {
    let mut module = WasmModule::with_store("boards", BOARDS, Box::new(MemStore::new())).unwrap();
    let mut env = TestCtx::at_height(1).env().clone();
    env.origin = Origin::External(vec![8; 32]);
    env.me = "boards".into();
    let mut author = TestCtx::with_env(env);
    let mut env = TestCtx::at_height(2).env().clone();
    env.origin = Origin::External(vec![3; 32]);
    env.me = "boards".into();
    let mut somebody_else = TestCtx::with_env(env);
    let msg = |payload: &[u8]| Msg {
        target: "boards".into(),
        payload: payload.to_vec(),
    };
    let listed = br#"{"list":null}"#;
    let opened = br#"{"get":{"id":"room"}}"#;

    module
        .execute(
            &mut author,
            &msg(br#"{"create":{"id":"room","title":"Q3 plannign"}}"#),
        )
        .await
        .unwrap();
    module
        .execute(
            &mut somebody_else,
            &msg(br#"{"rename":{"board":"room","title":"Q3 planning"}}"#),
        )
        .await
        .unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&module.query(opened).await.unwrap()).unwrap();
    assert_eq!(reply["board"]["title"], "Q3 planning");
    let catalogue: serde_json::Value =
        serde_json::from_slice(&module.query(listed).await.unwrap()).unwrap();
    assert_eq!(catalogue["list"]["room"], "Q3 planning");

    // A card on it, and it stops being anyone's to throw away.
    let shape = serde_json::json!({"kind":"note","x":0,"y":0,"width":200,"height":140,"text":"","color":0,"align":"middle","text_size":"medium","points":[],"from":null,"to":null});
    let draw =
        serde_json::json!({"edit":{"board":"room","change":{"create":{"id":"a","shape":shape}}}});
    module
        .execute(&mut author, &msg(&serde_json::to_vec(&draw).unwrap()))
        .await
        .unwrap();
    assert!(
        module
            .execute(&mut author, &msg(br#"{"remove":{"board":"room"}}"#))
            .await
            .is_err(),
        "the deployable bytes threw away a board with work on it"
    );

    module
        .execute(
            &mut author,
            &msg(br#"{"edit":{"board":"room","change":{"delete":{"id":"a"}}}}"#),
        )
        .await
        .unwrap();
    module
        .execute(&mut somebody_else, &msg(br#"{"remove":{"board":"room"}}"#))
        .await
        .unwrap();
    module.commit_block().await.unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&module.query(opened).await.unwrap()).unwrap();
    assert!(reply["board"].is_null(), "the board's state outlived it");
    let catalogue: serde_json::Value =
        serde_json::from_slice(&module.query(listed).await.unwrap()).unwrap();
    assert_eq!(
        catalogue["list"],
        serde_json::json!({}),
        "the catalogue still lists a board that is gone"
    );
}
