use chat_view::{boot_native, tick_native};
use ducktape_view_guest::testing::{answer, item, refuse};
use ducktape_view_guest::wire::{Frame, Request};
use serde_json::{Value, json};

fn request<'a>(frame: &'a Frame, kind: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| request.kind == kind)
        .expect(kind)
}
fn payload(request: &Request) -> Value {
    serde_json::from_slice(&request.payload).unwrap()
}
fn start(intent: Value) -> Frame {
    boot_native();
    let frame = tick_native(Vec::new());
    tick_native(vec![item(
        request(&frame, "chat.props").id,
        &serde_json::to_vec(&json!({"background":intent})).unwrap(),
    )])
}
fn proof(frame: &Frame, channel: &str) -> Frame {
    let request = request(frame, "rpc.admin");
    assert_eq!(
        payload(request),
        json!({"route":"/v1/huddle/node-proof","payload":{"channel_id":channel}})
    );
    tick_native(vec![answer(
        request.id,
        &serde_json::to_vec(&json!({"node":"aa".repeat(32),"node_proof":"bb".repeat(64)})).unwrap(),
    )])
}
fn on_stack(test: fn()) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(test)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn join_uses_the_node_proof_and_emits_completion_after_the_module_accepts() {
    on_stack(|| {
        let frame = proof(&start(json!({"kind":"join","channel":"room"})), "room");
        let operation = request(&frame, "op.submit");
        assert_eq!(
            payload(operation),
            json!({"target":"chat","payload":{"join_huddle":{
            "channel_id":"room","node":vec![0xaa;32],"node_proof":vec![0xbb;64]}}})
        );
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "host.finish")
        );
        let frame = tick_native(vec![answer(operation.id, br#"{"height":1}"#)]);
        assert_eq!(
            payload(request(&frame, "host.emit")),
            json!({"channel":"room"})
        );
        assert!(request(&frame, "host.finish").payload.is_empty());
    });
}

#[test]
fn move_leaves_before_joining_and_reports_a_committed_leave_when_join_is_refused() {
    on_stack(|| {
        let frame = start(json!({"kind":"move","from":"old","channel":"new"}));
        let leave = request(&frame, "op.submit");
        assert_eq!(
            payload(leave),
            json!({"target":"chat","payload":{"leave_huddle":{"channel_id":"old"}}})
        );
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.admin")
        );
        let frame = tick_native(vec![answer(leave.id, br#"{"height":1}"#)]);
        let mint = request(&frame, "rpc.admin");
        let frame = tick_native(vec![refuse(mint.id, "not seated")]);
        assert_eq!(
            payload(request(&frame, "host.emit")),
            json!({"error":{"message":"not seated","committed":true}})
        );
    });
}

#[test]
fn a_refused_leave_cannot_continue_to_join() {
    on_stack(|| {
        let frame = start(json!({"kind":"move","from":"old","channel":"new"}));
        let frame = tick_native(vec![refuse(
            request(&frame, "op.submit").id,
            "leave refused",
        )]);
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.admin")
        );
        assert_eq!(
            payload(request(&frame, "host.emit")),
            json!({"error":{"message":"leave refused","committed":false}})
        );
    });
}

#[test]
fn background_search_names_the_room_once_without_requesting_a_signature() {
    on_stack(|| {
        let frame = start(json!({"kind":"search", "channel":"room", "text":"needle"}));
        let directory = request(&frame, "rpc.query");
        assert_eq!(payload(directory)["target"], "identity");
        let frame = tick_native(vec![answer(directory.id, br#"{"accounts":[]}"#)]);
        let search = request(&frame, "rpc.view");
        assert_eq!(payload(search)["query"]["search"]["channel_id"], "room");
        let frame = tick_native(vec![answer(
            search.id,
            br#"{"hits":[{"channel_id":"room","seq":12,"author":"system","text":"needle"}]}"#,
        )]);
        let response = payload(request(&frame, "host.emit"));
        assert_eq!(response["hits"][0]["meta"], "room · #12");
        assert_eq!(response["hits"][0]["text"], "needle");
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "op.submit")
        );
        assert!(request(&frame, "host.finish").payload.is_empty());
    });
}

#[test]
fn notification_background_resolves_committed_mentions_and_reads_the_flat_channel_row() {
    on_stack(|| {
        let frame = start(json!({"kind":"notice","request":{
            "payload":{"post_message":{"channel_id":"room","message_id":"m","thread":null,
                "blocks":[{"paragraph":[{"text":"old","marks":[{"mention":{"key":vec![8;32]}}]}]}]}},
            "assigned":{"posted":{"seq":1,"actor":{"account":2},"key_mentions":[1]}},
            "context":{"key":vec![7;32],"screen":{"app_focused":false,"active_channel":""}}
        }}));
        let directory = request(&frame, "rpc.query");
        assert_eq!(payload(directory)["target"], "identity");
        let accounts = json!({"accounts":[
            {"number":1,"name":"Reader","keys":[{"pubkey":vec![7;32]}]},
            {"number":2,"name":"Reporter","keys":[]}
        ]});
        let frame = tick_native(vec![answer(
            directory.id,
            &serde_json::to_vec(&accounts).unwrap(),
        )]);
        let channel = request(&frame, "rpc.view");
        assert_eq!(
            payload(channel),
            json!({"target":"chat","query":{"channel":{"channel_id":"room"}}})
        );
        let frame = tick_native(vec![answer(
            channel.id,
            br#"{"channel":{"id":"room","name":"General","huddle":[]}}"#,
        )]);
        assert_eq!(
            payload(request(&frame, "host.emit")),
            json!({"notice":{
                "title":"#General","subtitle":"Reporter mentioned you","body":"@Reader","thread":"room"
            }})
        );
        assert!(request(&frame, "host.finish").payload.is_empty());
    });
}

#[test]
fn a_join_notification_reads_the_first_seat_and_names_it_in_the_view() {
    on_stack(|| {
        let frame = start(json!({"kind":"notice","request":{
            "payload":{"join_huddle":{"channel_id":"room"}},"assigned":null,
            "context":{"key":vec![7;32],"screen":{"app_focused":false,"active_channel":""}}
        }}));
        let channel = request(&frame, "rpc.view");
        let frame = tick_native(vec![answer(channel.id,br#"{"channel":{"id":"room","name":"General","huddle":[{"party":"acct:2","node":"aa"}]}}"#)]);
        let directory = request(&frame, "rpc.query");
        let frame = tick_native(vec![answer(
            directory.id,
            br#"{"accounts":[{"number":2,"name":"Reporter","keys":[]}]}"#,
        )]);
        assert_eq!(
            payload(request(&frame, "host.emit")),
            json!({"notice":{
                "title":"#General","subtitle":"Reporter started a huddle","body":"Join from the room list.","thread":"room"
            }})
        );
    });
}

#[test]
fn a_live_channel_fold_uses_cached_identity_and_only_the_index_lane() {
    on_stack(|| {
        let key = "07".repeat(32);
        let frame = start(json!({"kind":"channel","channel":"room","key":key,
            "names":{"accounts":{key:{"number":1,"name":"Reader"}},"by_account":{"1":"Reader","2":"Peer"},"programs":[]}
        }));
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.query")
        );
        let channel = request(&frame, "rpc.view");
        assert_eq!(
            payload(channel),
            json!({"target":"chat","query":{"channel":{"channel_id":"room"}}})
        );
        let frame = tick_native(vec![answer(channel.id,br#"{"channel":{"id":"room","name":"General","post_policy":"members_only","head_seq":9,"huddle":[{"party":"acct:1","node":"aa","joined_at":4},{"party":"acct:2","node":"bb","joined_at":5}]}}"#)]);
        let response = payload(request(&frame, "host.emit"));
        assert_eq!(response["channel"][0]["members_only"], true);
        let roster = &response["channel"][1];
        assert_eq!(roster[0]["label"], "Reader");
        assert_eq!(roster[0]["key"], "acct:1");
        assert_eq!(roster[0]["node"], "aa");
        assert_eq!(roster[0]["joined_at"], 4);
        assert_eq!(roster[0]["is_you"], true);
        assert_eq!(roster[1]["label"], "Peer");
        assert_eq!(roster[1]["is_you"], false);
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.query")
        );
    });
}

#[test]
fn live_discovery_selects_chat_runs_and_keeps_missing_agent_names_cached() {
    on_stack(|| {
        let frame = start(json!({"kind":"live_runs","labels":{"known":"Known agent"}}));
        let pending = request(&frame, "rpc.query");
        assert_eq!(
            payload(pending),
            json!({"target":"runs","query":"pending_runs"})
        );
        let frame = tick_native(vec![answer(
            pending.id,
            &serde_json::to_vec(&json!({"pending_runs":[
                {"dispatch_id":"job","channel_id":"","agent_id":"job-agent"},
                {"dispatch_id":"chat","channel_id":"room","agent_id":"missing"}
            ]}))
            .unwrap(),
        )]);
        let roster = request(&frame, "rpc.query");
        assert_eq!(
            payload(roster),
            json!({"target":"runs","query":{"model":{"query":"agents"}}})
        );
        let frame = tick_native(vec![refuse(roster.id, "roster unavailable")]);
        let result = payload(request(&frame, "host.emit"));
        assert_eq!(result["records"].as_array().unwrap().len(), 1);
        assert_eq!(result["records"][0]["dispatch_id"], "chat");
        assert_eq!(
            result["labels"],
            json!({"known":"Known agent","missing":"missing"})
        );

        let frame = start(json!({"kind":"live_runs","labels":result["labels"]}));
        let pending = request(&frame, "rpc.query");
        let frame = tick_native(vec![answer(
            pending.id,
            &serde_json::to_vec(&json!({"pending_runs":result["records"]})).unwrap(),
        )]);
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.query")
        );
        assert_eq!(
            payload(request(&frame, "host.emit"))["labels"],
            result["labels"]
        );
    });
}

#[test]
fn public_run_progress_queries_once_and_emits_only_public_facts() {
    on_stack(|| {
        let frame = start(json!({"kind":"run_progress","runs":["one","two"]}));
        let sessions = request(&frame, "rpc.query");
        assert_eq!(
            payload(sessions),
            json!({"target":"runs","query":"agent_sessions"})
        );
        let frame = tick_native(vec![answer(sessions.id, br#"{"agent_sessions":[{"run_id":"one","actions":3,"session_key":"SECRET"},{"run_id":"unrelated","actions":99}]}"#)]);
        let reads: Vec<_> = frame
            .requests
            .iter()
            .filter(|request| request.kind == "rpc.query")
            .collect();
        assert_eq!(reads.len(), 2);
        let replies = reads
            .iter()
            .map(|read| {
                let query = payload(read);
                assert_eq!(query["target"], "runs");
                let run = query["query"]["delegations"]["caller_run_id"]
                    .as_str()
                    .unwrap();
                match run {
                    "one" => answer(
                        read.id,
                        br#"{"delegations":[{"status":"pending","result":"SECRET"}]}"#,
                    ),
                    "two" => refuse(read.id, "unavailable"),
                    _ => panic!("only requested runs are queried"),
                }
            })
            .collect();
        let frame = tick_native(replies);
        let progress = payload(request(&frame, "host.emit"));
        assert_eq!(
            progress["one"]["sessions"]["agent_sessions"],
            json!([{"run_id":"one","actions":3}])
        );
        assert_eq!(
            progress["one"]["delegations"]["delegations"],
            json!([{"status":"pending"}])
        );
        assert_eq!(progress["two"]["sessions"]["agent_sessions"], json!([]));
        assert!(progress["two"]["delegations"]["delegations"].is_null());
        assert!(!progress.to_string().contains("SECRET"));
        assert!(progress.get("unrelated").is_none());
    });
}

#[test]
fn shell_posts_use_assigned_heads_without_fetching_or_rendering_message_bodies() {
    on_stack(|| {
        for thread in [Value::Null, json!(3)] {
            let frame = start(json!({"kind":"shell_delta", "key":"", "names":{},
                "payload":{"post_message":{"channel_id":"room","thread":thread,"blocks":[]}},
                "assigned":{"posted":{"seq":7}}
            }));
            assert_eq!(
                payload(request(&frame, "host.emit")),
                json!({"delta":{"head":{"channel_id":"room","seq":7}}})
            );
            assert!(
                !frame
                    .requests
                    .iter()
                    .any(|request| request.kind.starts_with("rpc."))
            );
        }
        let frame = start(json!({"kind":"shell_delta", "key":"", "names":{},
            "payload":{"post_message":{"channel_id":"room"}},"assigned":null}));
        assert!(payload(request(&frame, "host.emit"))["error"].is_object());
    });
}

#[test]
fn shell_channel_changes_read_the_current_row_and_dm_uses_the_assigned_id() {
    on_stack(|| {
        for (operation, assigned) in [
            (
                json!({"rename_channel":{"channel_id":"room","name":"new"}}),
                Value::Null,
            ),
            (json!({"join_huddle":{"channel_id":"room"}}), Value::Null),
            (
                json!({"create_dm_channel":{"counterpart":2}}),
                json!({"dm_channel":{"channel_id":"room"}}),
            ),
        ] {
            let frame = start(
                json!({"kind":"shell_delta","payload":operation,"assigned":assigned,
                "key":"", "names":{"accounts":{},"by_account":{},"programs":[]}}),
            );
            let read = request(&frame, "rpc.view");
            assert_eq!(
                payload(read),
                json!({"target":"chat","query":{"channel":{"channel_id":"room"}}})
            );
            let frame = tick_native(vec![answer(
                read.id,
                br#"{"channel":{"id":"room","name":"new","head_seq":9,"huddle":[]}}"#,
            )]);
            let result = payload(request(&frame, "host.emit"));
            assert_eq!(result["delta"]["channel"]["channel"]["name"], "new");
            assert_eq!(result["delta"]["channel"]["channel"]["head_seq"], 9);
        }
    });
}

#[test]
fn view_only_changes_do_not_send_message_payloads_to_the_shell() {
    on_stack(|| {
        for operation in [
            "edit_message",
            "delete_message",
            "add_reaction",
            "set_membership",
            "register_hook",
        ] {
            let frame = start(json!({"kind":"shell_delta", "key":"", "names":{},
                "payload":{operation:{"channel_id":"room"}},"assigned":null}));
            assert_eq!(payload(request(&frame, "host.emit")), json!({"delta":null}));
        }
    });
}
