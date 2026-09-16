//! Loads independently built application artifacts from files after this test
//! binary is compiled. The example isn't linked into the host's module set.
use sdk::{Ctx, Env, Error, Event, Module, Msg, Origin, StateRoot};
use wasm_host::WasmModule;

struct Caller(Env);
impl Caller {
    fn account(account: u8) -> Self {
        Self(Env {
            height: 1,
            consensus_time: 0,
            origin: Origin::External(vec![account]),
            me: "unlisted-application".into(),
            cause: sdk::Cause::Direct,
        })
    }
}
#[async_trait::async_trait(?Send)]
impl Ctx for Caller {
    fn env(&self) -> &Env {
        &self.0
    }
    fn module_root(&self, _: &str) -> Option<StateRoot> {
        None
    }
    async fn query(&self, target: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        assert_eq!(target, "identity");
        let query: serde_json::Value = serde_json::from_slice(request).unwrap();
        let account = query["of_key"]["key"][0].as_u64().unwrap();
        Ok(serde_json::to_vec(&serde_json::json!({"account":{"number":account}})).unwrap())
    }
    fn emit_msg(&mut self, _: Msg) {
        panic!("example emits no cross-module writes")
    }
    fn emit_event(&mut self, event: Event) {
        assert_eq!(event.source, "changed")
    }
}
async fn execute(
    module: &mut WasmModule,
    account: u8,
    payload: serde_json::Value,
) -> Result<(), Error> {
    let mut caller = Caller::account(account);
    module
        .execute(
            &mut caller,
            &Msg {
                target: module.id(),
                payload: serde_json::to_vec(&payload).unwrap(),
            },
        )
        .await
}

#[tokio::test]
#[ignore = "build crates/examples/extension-probe/build.py artifacts after compiling this test binary"]
async fn file_loaded_policy_replacement_and_restart_preserve_state_and_change_authorization() {
    let artifacts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/extension-probe/artifacts");
    let original =
        std::fs::read(artifacts.join("module.component.wasm")).expect("build example artifacts");
    let replacement = std::fs::read(artifacts.join("module-replacement.component.wasm")).unwrap();
    let mut module = WasmModule::from_bytes("unlisted-application", &original).unwrap();
    execute(
        &mut module,
        1,
        serde_json::json!({"configure":{"members":[1,9]}}),
    )
    .await
    .unwrap();
    module.commit_block().await.unwrap();
    execute(
        &mut module,
        9,
        serde_json::json!({"record":{"text":"before"}}),
    )
    .await
    .unwrap();
    module.commit_block().await.unwrap();
    let before = module.root();
    assert!(
        execute(
            &mut module,
            8,
            serde_json::json!({"record":{"text":"outsider"}})
        )
        .await
        .is_err()
    );
    module.abort_block().await.unwrap();
    assert_eq!(module.root(), before);
    let query =
        serde_json::to_vec(&serde_json::json!({"authorize":{"account":9,"text":"hello"}})).unwrap();
    assert_eq!(module.query(&query).await.unwrap(), b"true");
    let old_code = module.code_hash();
    let component = replacement.clone();
    let replacement = module_artifact::Artifact::module(replacement).encode();
    module.swap_code(&replacement).unwrap();
    assert_eq!(module.root(), before);
    assert_ne!(module.code_hash(), old_code);
    assert_eq!(module.query(&query).await.unwrap(), b"false");
    assert!(
        execute(
            &mut module,
            9,
            serde_json::json!({"record":{"text":"rejected"}})
        )
        .await
        .is_err()
    );
    module.abort_block().await.unwrap();
    execute(
        &mut module,
        9,
        serde_json::json!({"record":{"text":"#after"}}),
    )
    .await
    .unwrap();
    module.commit_block().await.unwrap();
    let state = module.query(br#""state""#).await.unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(decoded["count"], 2);
    assert_eq!(decoded["last"], "#after");
    let root = module.root();
    let snapshot = module.snapshot();
    let code = module.code_hash();
    drop(module);
    let mut restarted = WasmModule::from_bytes("unlisted-application", &component).unwrap();
    restarted.install(&snapshot, root).unwrap();
    assert_eq!(restarted.root(), root);
    assert_eq!(restarted.code_hash(), code);
    assert_eq!(restarted.query(br#""state""#).await.unwrap(), state);
    assert_eq!(restarted.query(&query).await.unwrap(), b"false");
}
