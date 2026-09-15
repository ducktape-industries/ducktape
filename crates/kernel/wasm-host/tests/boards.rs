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
    let shape = serde_json::json!({"kind":"note","x":0,"y":0,"width":200,"height":140,"text":"","color":0,"from":null,"to":null});
    let operations = [
        serde_json::json!({"create":{"id":"room","title":"Planning"}}),
        serde_json::json!({"edit":{"board":"room","change":{"create":{"id":"a","shape":shape}}}}),
        serde_json::json!({"edit":{"board":"room","change":{"move":{"id":"a","x":100,"y":50}}}}),
        serde_json::json!({"edit":{"board":"room","change":{"text":{"id":"a","text":"같이 생각하기"}}}}),
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
