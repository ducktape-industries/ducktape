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
