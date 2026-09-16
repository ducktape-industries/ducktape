//! Exercise the committed guests together: native wire types alone cannot
//! detect a caller component still carrying an older sibling reply decoder.
use commonware_runtime::Runner as _;
use host::{BlockContext, Host};
use noded::bundle::{DirCodeSource, qmdb_stores};
use noded::compose::{Bindings, Boot, Substrates, compose};
use sdk::{Msg, Origin};
use serde_json::{Value, json};

async fn submit(host: &mut Host, height: &mut u64, key: u8, target: &str, body: Value) {
    *height += 1;
    host.submit_at(
        BlockContext {
            height: *height,
            consensus_time: *height,
            origin: Origin::External(vec![key; 32]),
        },
        Msg {
            target: target.into(),
            payload: sdk::wire::encode(&body),
        },
    )
    .await
    .unwrap();
}

async fn drain(host: &mut Host, height: &mut u64) {
    while host.has_pending_work().await.unwrap() {
        *height += 1;
        host.submit_block(
            BlockContext {
                height: *height,
                consensus_time: *height,
                origin: Origin::System,
            },
            Vec::new(),
        )
        .await
        .unwrap();
    }
}

async fn query(host: &Host, target: &str, body: Value) -> Value {
    let bytes = host.query(target, &sdk::wire::encode(&body)).await.unwrap();
    sdk::wire::decode(&bytes).unwrap()
}

#[test]
fn committed_guests_deliver_a_model_reply_to_chat() {
    std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
        let scratch = tempfile::tempdir().unwrap();
        let config = commonware_runtime::tokio::Config::default()
            .with_storage_directory(scratch.path().join("storage"));
        commonware_runtime::tokio::Runner::new(config).start(|context| async move {
            let dir = workspace_config::modules_dir().unwrap();
            let (code, bundle) = DirCodeSource::open(
                &dir, &topology::TOPOLOGY.wasm_ids(topology::PRODUCTION),
            ).unwrap();
            let mut stores = qmdb_stores(&context);
            let substrates = Substrates {
                directory: scratch.path().join("module-storage"),
                bindings: [("forge".into(), scratch.path().join("forge")), ("files".into(), scratch.path().join("duckfs"))].into(),
                blobs: blobstore::BlobHandle::default(),
            };
            let bindings = Bindings {
                invite: b"model-reply-test", chain_id: "model-reply-test",
                time_unit: sdk::genesis_config::TimeUnit::Height,
            };
            let mut host = compose(&code, &mut stores, &substrates, &bindings,
                Boot::Genesis { validators: &[vec![8; 32], vec![7; 32]], bundle: &bundle },
            ).await.unwrap();
            let mut height = 0;
            submit(&mut host, &mut height, 8, "capability", json!({"announce": {
                "capabilities": ["model-1"], "resources": {}
            }})).await;
            submit(&mut host, &mut height, 1, "identity", json!({"create": {
                "name": "Alice", "scheme": "ed25519"
            }})).await;
            submit(&mut host, &mut height, 1, "agent", json!({"provision": {
                "request_id": "provision-builder", "name": "Builder",
                "program": runs::model_program("builder")
            }})).await;
            submit(&mut host, &mut height, 1, "runs", json!({"configure_model": {
                "operation": {"register_model": {
                    "account": 2, "agent_id": "builder", "display_name": "Builder",
                    "capability": "model-1", "recipe_hash": null, "skills": null
                }}
            }})).await;
            submit(&mut host, &mut height, 1, "chat", json!({"create_channel": {
                "channel_id": "general", "name": "General", "post_policy": "open"
            }})).await;
            submit(&mut host, &mut height, 1, "chat", json!({"post_message": {
                "channel_id": "general", "message_id": "mention", "thread": null,
                "blocks": [{"paragraph": [{"text": "Builder, say hello",
                    "marks": [{"mention": {"account": 2}}]}]}]
            }})).await;
            drain(&mut host, &mut height).await;
            let pending = query(&host, "runs", json!("pending_runs")).await;
            let entries = pending["pending_runs"].as_array().unwrap();
            assert_eq!(entries.len(), 1, "{pending}");
            let run = &entries[0];
            let dispatch = query(&host, "dispatch", json!({"dispatch": {
                "receiver": "runs", "dispatch_id": run["dispatch_id"]
            }})).await;
            let saga = &dispatch["dispatch"]["status"]["awaiting_result"]["saga_id"];
            assert!(saga.is_string(), "{dispatch}");
            submit(&mut host, &mut height, 8, "saga", json!({"accept": {
                "saga_id": saga, "attempt": 0
            }})).await;
            let response = json!({"reply_blocks": [{"id":"answer", "kind":"paragraph", "text":"PONG"}],
                "actions": [], "commit_message": null});
            let output = sdk::wire::encode(&json!({
                "ducktape_runner_result": 1, "response_text": response.to_string(),
                "workspace_receipt": {"source_prefix":"/shared/agent-workspaces/builder",
                    "output_snapshot":null, "commit_height":null, "rebased":false, "no_changes":true}
            }));
            submit(&mut host, &mut height, 8, "saga", json!({"oracle_result": {
                "saga_id": saga, "attempt": 0, "outcome": {"Ok":output}, "usage":null
            }})).await;
            drain(&mut host, &mut height).await;
            let recent = query(&host, "runs", json!("recent_runs")).await;
            let settled = query(&host, "dispatch", json!({"dispatch": {
                "receiver": "runs", "dispatch_id": run["dispatch_id"]
            }})).await;
            assert_eq!(recent["recent_runs"][0]["outcome"], "result_accepted", "{recent}; {settled}");
            let channel = query(&host, "chat", json!({"channel":{"channel_id":"general"}})).await;
            assert_eq!(channel["channel"]["head_seq"], 2, "{channel}");
            let posted = query(&host, "chat", json!({"messages_range": {
                "channel_id":"general", "from_seq":2, "limit":1
            }})).await;
            assert_eq!(posted["messages"][0]["head"]["blocks"][0]["paragraph"][0]["text"], "PONG", "{posted}");
        });
    }).unwrap().join().unwrap();
}
