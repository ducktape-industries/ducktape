//! Registry ports preserve state roots and their block-boundary decisions.
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use sdk::{Env, Module, Msg, Origin};
use sdk_testkit::{MemStore, TestCtx};
use wasm_host::WasmModule;

const VALSET: &[u8] = include_bytes!("fixtures/valset.component.wasm");
const MODULES: &[u8] = include_bytes!("fixtures/modules.component.wasm");

fn key(seed: u64) -> Vec<u8> {
    PrivateKey::from_seed(seed).public_key().as_ref().to_vec()
}

async fn valset_replies(module: &dyn Module) -> Vec<Vec<u8>> {
    let mut replies = Vec::new();
    for query in [
        valset::ValsetQuery::Validators,
        valset::ValsetQuery::Residents,
        valset::ValsetQuery::MeshWindow,
    ] {
        replies.push(module.query(&valset::encode_query(&query)).await.unwrap());
    }
    replies
}

#[test]
fn valset_initialization_finalization_net_zero_and_abort_match_native() {
    futures::executor::block_on(async {
        let mut native = valset::Valset::new("valset", Box::new(MemStore::new()), "governance");
        let mut wasm = WasmModule::with_store("valset", VALSET, Box::new(MemStore::new())).unwrap();
        let params = sdk::genesis_config::encode_config(&[(
            "validators",
            &sdk::wire::encode(&vec![key(1), key(2)]),
        )]);
        native.initialize(&params).await.unwrap();
        wasm.initialize(&params).await.unwrap();
        assert_eq!(native.root(), wasm.root());
        assert_eq!(valset_replies(&native).await, valset_replies(&wasm).await);
        let genesis_root = wasm.root();
        wasm.initialize(&params).await.unwrap();
        assert_eq!(wasm.root(), genesis_root, "initialization is idempotent");

        let blocks = [
            vec![
                valset::ValsetMsg::Join { key: key(3) },
                valset::ValsetMsg::Grant { key: key(4) },
            ],
            vec![
                valset::ValsetMsg::Leave { key: key(3) },
                valset::ValsetMsg::Join { key: key(3) },
            ],
            vec![valset::ValsetMsg::Join { key: key(4) }],
        ];
        for (height, operations) in blocks.into_iter().enumerate() {
            let before = native.root();
            for operation in operations {
                let msg = Msg {
                    target: "valset".into(),
                    payload: valset::encode_msg(&operation),
                };
                native
                    .execute(&mut TestCtx::at_height(height as u64 + 1), &msg)
                    .await
                    .unwrap();
                wasm.execute(&mut TestCtx::at_height(height as u64 + 1), &msg)
                    .await
                    .unwrap();
                assert_eq!(native.root(), before);
                assert_eq!(wasm.root(), before);
                assert_eq!(valset_replies(&native).await, valset_replies(&wasm).await);
            }
            native.commit_block().await.unwrap();
            wasm.commit_block().await.unwrap();
            assert_eq!(native.root(), wasm.root());
            assert_eq!(valset_replies(&native).await, valset_replies(&wasm).await);
        }
        let query = valset::encode_query(&valset::ValsetQuery::MeshWindow);
        let valset::ValsetReply::MeshWindow(window) =
            valset::decode_reply(&wasm.query(&query).await.unwrap()).unwrap()
        else {
            panic!("mesh window reply");
        };
        assert_eq!(
            window.last().unwrap().generation,
            2,
            "the net-zero block burns no generation"
        );

        let before = wasm.root();
        let msg = Msg {
            target: "valset".into(),
            payload: valset::encode_msg(&valset::ValsetMsg::Leave { key: key(4) }),
        };
        native
            .execute(&mut TestCtx::at_height(4), &msg)
            .await
            .unwrap();
        wasm.execute(&mut TestCtx::at_height(4), &msg)
            .await
            .unwrap();
        native.abort_block().await.unwrap();
        wasm.abort_block().await.unwrap();
        assert_eq!(native.root(), before);
        assert_eq!(wasm.root(), before);
        assert_eq!(valset_replies(&native).await, valset_replies(&wasm).await);
    });
}

fn registry_ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        cause: sdk::Cause::Direct,
        me: "modules".into(),
        origin,
    })
    .on_query("valset", |request| {
        let reply = match valset::decode_query(request).unwrap() {
            valset::ValsetQuery::Validators => valset::ValsetReply::Validators(vec![key(1)]),
            valset::ValsetQuery::Residents => valset::ValsetReply::Residents(Vec::new()),
            valset::ValsetQuery::MeshWindow => panic!("readiness only needs membership"),
        };
        Ok(valset::encode_reply(&reply))
    })
}

async fn registry_reply(module: &dyn Module) -> Vec<u8> {
    module
        .query_with(
            &TestCtx::at_height(0),
            &modules::encode_query(&modules::ModulesQuery::ModuleStatus),
        )
        .await
        .unwrap()
}

#[test]
fn registry_staged_queries_and_committed_advance_match_native() {
    futures::executor::block_on(async {
        let mut native =
            modules::Modules::new("modules", Box::new(MemStore::new()), "valset", "governance");
        let mut wasm =
            WasmModule::with_store("modules", MODULES, Box::new(MemStore::new())).unwrap();
        let seed = |kind, byte| modules::Seed {
            kind,
            code_hash: vec![byte; 32],
            lanes: Vec::new(),
        };
        let roster = std::collections::BTreeMap::from([
            ("weather", seed(modules::Kind::Module, 1u8)),
            ("modules", seed(modules::Kind::Module, 2u8)),
            ("home", seed(modules::Kind::View, 4u8)),
        ]);
        let params =
            sdk::genesis_config::encode_config(&[("modules", &sdk::wire::encode(&roster))]);
        native.initialize(&params).await.unwrap();
        wasm.initialize(&params).await.unwrap();
        assert_eq!(native.root(), wasm.root());
        let messages = [
            (
                Origin::System,
                modules::ModulesMsg::ScheduleSwap {
                    name: "weather-update".into(),
                    module_id: "weather".into(),
                    activation_height: 5,
                    code_hash: vec![3; 32],
                },
            ),
            (
                Origin::External(key(1)),
                modules::ModulesMsg::SwapReady {
                    name: "weather-update".into(),
                    module_id: "weather".into(),
                    code_hash: vec![3; 32],
                },
            ),
            // a view entry swaps through the same machinery, kind kept.
            (
                Origin::System,
                modules::ModulesMsg::ScheduleSwap {
                    name: "home-update".into(),
                    module_id: "home".into(),
                    activation_height: 5,
                    code_hash: vec![5; 32],
                },
            ),
            (
                Origin::External(key(1)),
                modules::ModulesMsg::SwapReady {
                    name: "home-update".into(),
                    module_id: "home".into(),
                    code_hash: vec![5; 32],
                },
            ),
            (
                Origin::System,
                modules::ModulesMsg::ScheduleRegister {
                    name: "admit-board".into(),
                    module_id: "board".into(),
                    kind: modules::Kind::View,
                    activation_height: 5,
                    code_hash: vec![6; 32],
                    lanes: Vec::new(),
                },
            ),
            (
                Origin::External(key(1)),
                modules::ModulesMsg::SwapReady {
                    name: "admit-board".into(),
                    module_id: "board".into(),
                    code_hash: vec![6; 32],
                },
            ),
        ];
        for (origin, operation) in messages {
            let msg = Msg {
                target: "modules".into(),
                payload: modules::encode_msg(&operation),
            };
            native
                .execute(&mut registry_ctx(1, origin.clone()), &msg)
                .await
                .unwrap();
            wasm.execute(&mut registry_ctx(1, origin), &msg)
                .await
                .unwrap();
            assert_eq!(registry_reply(&native).await, registry_reply(&wasm).await);
        }
        native.commit_block().await.unwrap();
        wasm.commit_block().await.unwrap();
        assert_eq!(native.root(), wasm.root());
        assert_eq!(registry_reply(&native).await, registry_reply(&wasm).await);
        for height in [2, 5] {
            let msg = Msg {
                target: "modules".into(),
                payload: modules::encode_msg(&modules::ModulesMsg::Advance),
            };
            native
                .execute(&mut registry_ctx(height, Origin::System), &msg)
                .await
                .unwrap();
            wasm.execute(&mut registry_ctx(height, Origin::System), &msg)
                .await
                .unwrap();
            assert_eq!(registry_reply(&native).await, registry_reply(&wasm).await);
            native.commit_block().await.unwrap();
            wasm.commit_block().await.unwrap();
            assert_eq!(native.root(), wasm.root());
            assert_eq!(registry_reply(&native).await, registry_reply(&wasm).await);
        }
        let modules::ModulesReply::ModuleStatus { modules } =
            modules::decode_reply(&registry_reply(&wasm).await).unwrap()
        else {
            panic!("registry status reply");
        };
        let weather = modules
            .iter()
            .find(|module| module.module_id == "weather")
            .unwrap();
        assert_eq!(weather.active_code_hash, vec![3; 32]);
        assert_eq!(weather.kind, modules::Kind::Module);
        assert_eq!(weather.history.last().unwrap().height, 5);
        let home = modules
            .iter()
            .find(|module| module.module_id == "home")
            .unwrap();
        assert_eq!(home.kind, modules::Kind::View);
        assert_eq!(home.active_code_hash, vec![5; 32]);
        let board = modules
            .iter()
            .find(|module| module.module_id == "board")
            .unwrap();
        assert_eq!(board.kind, modules::Kind::View);
        assert_eq!(board.active_code_hash, vec![6; 32]);
        assert_eq!(board.history.len(), 1);
    });
}
