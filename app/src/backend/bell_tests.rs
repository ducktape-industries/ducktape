use super::*;
use serde_json::{Value, json};

fn item(seq: i64, reason: &str, actor: &str) -> BellItem {
    BellItem {
        seq,
        reason: reason.into(),
        actor: actor.into(),
        kind: "added".into(),
        source: format!("chat/message/m{seq}"),
        ..BellItem::default()
    }
}

#[test]
fn bell_loaded_and_live_rows_share_filter_count_and_full_retention() {
    let loaded: Vec<_> = (1..=300)
        .rev()
        .map(|seq| item(seq, "mention", "account:9"))
        .collect();
    let visible = bell_visible_items(&loaded, "4", "");
    assert_eq!((visible[0].seq, visible[49].seq), (300, 251));
    assert_eq!(bell_unread_count(&loaded, "4", ""), 300);
    let delta = BellDelta {
        kind: "delivered".into(),
        item: item(301, "authorship", "account:4"),
        ..BellDelta::default()
    };
    let rows = apply_bell(loaded, delta.clone());
    let duplicate = apply_bell(rows.clone(), delta);
    assert_eq!(rows, duplicate);
    assert_eq!(rows.len(), 301);
    assert_eq!(bell_unread_count(&rows, "4", ""), 300);
    assert_eq!(bell_visible_items(&rows, "4", "")[0].seq, 300);
    assert_eq!(
        bell_unread_count(&[item(1, "mention", "account:4")], "4", ""),
        1,
        "a real self mention is not authorship noise"
    );
    assert_eq!(
        bell_unread_count(&[item(1, "ownership", "account:9")], "4", ""),
        1,
        "another actor's ownership event remains visible"
    );
}

#[test]
fn bell_read_watermark_preserves_new_arrivals_and_cannot_be_undone_by_late_load() {
    let before = vec![
        item(2, "mention", "account:9"),
        item(1, "mention", "account:9"),
    ];
    let mut current = before.clone();
    current.insert(0, item(3, "mention", "account:9"));
    let read = BellDelta {
        kind: "read".into(),
        up_to_seq: 2,
        ..BellDelta::default()
    };
    let current = apply_bell(current, read);
    assert!(!current[0].read);
    assert!(current[1..].iter().all(|item| item.read));
    let merged = merge_bell_loaded(current, before, 2, 1);
    assert_eq!(
        merged.iter().map(|item| item.seq).collect::<Vec<_>>(),
        [3, 2]
    );
    assert_eq!(bell_unread_count(&merged, "4", ""), 1);
    assert!(merged[1].read);
}

#[test]
fn bell_context_names_people_without_inventing_content_edits() {
    let names = NameDirectory::new(
        [(
            String::from("aa"),
            ::chat::client::BoundAccount {
                number: 9,
                name: "Alice".into(),
            },
        )]
        .into(),
    );
    let mut row = item(1, "mention", "account:9");
    assert_eq!(bell_actor(&row, &names), "Alice");
    assert_eq!(bell_summary(&row, "Alice").title, "Alice mentioned you");
    row.kind = "withdrawn".into();
    assert_eq!(bell_summary(&row, "Alice").title, "Mention changed · Alice");
    row.kind = "added".into();
    row.reason = "authorship".into();
    assert_eq!(
        bell_summary(&row, "Alice").title,
        "Activity on your post · Alice"
    );
    assert!(!bell_summary(&row, "Alice").detail.contains("m1"));
    let known = vec![BellPresentation {
        seq: 1,
        ..BellPresentation::default()
    }];
    assert!(bell_missing_items(vec![row], &known).is_empty());
}

async fn server(reply: impl Fn(Value) -> Value + Send + Sync + 'static) -> RpcClient {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let reply = std::sync::Arc::new(reply);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                        continue;
                    };
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
                    }
                };
                let body = reply(body).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    rpc_client(&url).unwrap()
}

#[tokio::test]
async fn bell_loader_walks_past_the_first_index_page() {
    let rpc = server(|query| {
        let from = query["list"]["from_seq"].as_u64().expect("the actual inbox cursor");
        let rows: Vec<_> = (1u64..=300).filter(|seq| *seq >= from).take(256).map(|seq| json!({
            "seq":seq,"account":4,"height":seq,"created_at":0,"read":false,
            "change":{"seq":seq,"source":{"module":"chat","kind":"message","object":format!("m{seq}")},
              "revision":1,"recipient":4,"reason":"mention","kind":"added","actor":{"account":9},"cause":"Direct","height":seq}
        })).collect();
        json!({"items":rows})
    }).await;
    let rows = load_bell_items(&rpc, 4).await.unwrap();
    assert_eq!(rows.len(), 300);
    assert_eq!(
        bell_visible_items(&rows, "4", "")[0].seq,
        300,
        "latest notification, not the first page's newest"
    );
}

#[tokio::test]
async fn bell_message_preview_and_navigation_use_the_resolved_source() {
    let rpc = server(|query| {
        assert_eq!(query["message"]["message_id"], "m17");
        json!({"message":{"channel_id":"general","seq":42,"message_id":"m17","author":"acct:9", "height":1,"time":1,
          "blocks":[],"text":"Please review the launch checklist.","deleted":false,"edited":false,"rev":0,"edited_at":null,
          "base_rev":null,"thread":null,"reply_count":0,"last_reply_seq":null,"reactions":[],"tags":[]}})
    }).await;
    let row = item(17, "mention", "account:9");
    let mut entry = bell_summary(&row, "Alice");
    bell_source(&rpc, &row, &mut entry).await.unwrap();
    assert_eq!(entry.detail, "Please review the launch checklist.");
    assert_eq!(
        entry.number, 42,
        "inbox sequence17 is not message sequence42"
    );
    assert_eq!(
        bell_link(&entry, "demo#d0cdf950".into()),
        "duck://channel/general?net=d0cdf950#42"
    );
    assert!(bell_openable(&row, &[entry]));
}

// A finite RPC script: assertions and socket tasks are joined, never detached.
async fn scripted_server(script: Vec<(Value, Value)>) -> (RpcClient, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = rpc_client(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        for (expected, reply) in script {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let body: Value = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let header = String::from_utf8_lossy(&bytes[..end]);
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap();
                let complete = bytes.len() >= end + 4 + length;
                if complete {
                    break serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
                }
            };
            assert_eq!(body, expected);
            let body = reply.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (rpc, task)
}

fn job_event_fixture() -> (BellItem, attribution::ChangeEntry) {
    let object = "a".repeat(64);
    let row = BellItem {
        change_seq: 42,
        source: format!("tasks/job_event/{object}"),
        ..item(17, "ownership", "account:9")
    };
    let detail = ::tasks::JobEventDetail {
        job_id: "resolved-job".into(),
        conversation_id: "conversation".into(),
        job_kind: "native_worker".into(),
        created_at_revision: 3,
        job_attempt: 1,
        submitter: ::tasks::Party::Account(4),
        actor: ::tasks::Party::Account(9),
        height: 8,
        operation: ::tasks::JobsMsg::Cancel {
            job_id: "resolved-job".into(),
        },
    };
    let record = attribution::ChangeEntry {
        at: 42,
        change: attribution::Change {
            seq: 42,
            source: attribution::Source {
                module: "tasks".into(),
                kind: "job_event".into(),
                object,
            },
            revision: 1,
            recipient: 4,
            reason: attribution::Reason::Ownership,
            kind: attribution::ChangeKind::Added,
            detail: sdk::wire::encode(&detail),
            actor: attribution::Actor::Account(9),
            cause: sdk::Cause::Direct,
            height: 8,
        },
    };
    (row, record)
}

fn changes_request() -> Value {
    json!({"target":"attribution", "query":{"changes":{"after":41,"limit":1}}})
}

fn job_request() -> Value {
    json!({"target":"tasks", "query":{"job":{"get":{"job_id":"resolved-job"}}}})
}

fn job_reply() -> Value {
    json!({"job":{"job":{
        "job_id":"resolved-job", "execution":"conversation", "conversation_id":"conversation",
        "previous_job_id":null, "continuation_operation_id":null, "controls":[], "reports":[],
        "native_history":null, "kind":"native_worker", "spec":"", "submitter":{"account":4},
        "status":"done", "attempt":1, "claim":null, "result":null, "comments":[],
        "created_at_revision":3, "created_at_height":7, "updated_at_height":9
    }}})
}

#[tokio::test]
async fn bell_job_event_resolves_detail_job_id_and_matches_ordinary_job_preview() {
    let (row, record) = job_event_fixture();
    let reply = serde_json::to_value(attribution::AttributionReply::Changes(vec![record])).unwrap();
    let (rpc, server) = scripted_server(vec![
        (changes_request(), reply),
        (job_request(), job_reply()),
        (job_request(), job_reply()),
    ])
    .await;
    let mut event = bell_summary(&row, "Alice");
    bell_source(&rpc, &row, &mut event).await.unwrap();
    let ordinary_row = BellItem {
        source: "tasks/job/resolved-job".into(),
        ..row.clone()
    };
    let mut ordinary = bell_summary(&ordinary_row, "Alice");
    bell_source(&rpc, &ordinary_row, &mut ordinary)
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(event, ordinary);
    assert_eq!(event.detail, "Native worker · Done");
    assert_eq!(event.target, BellTarget::Unavailable);
    assert!(event.object.is_empty());
    assert!(bell_link(&event, "demo#d0cdf950".into()).is_empty());
    assert!(!bell_openable(&row, &[event]));
}

#[tokio::test]
async fn bell_job_event_rejects_missing_mismatched_and_corrupt_changes() {
    let (row, record) = job_event_fixture();
    let mut cases = vec![(Vec::new(), "change not found".to_string())];
    for field in ["seq", "at", "module", "kind", "object", "detail"] {
        let mut wrong = record.clone();
        match field {
            "seq" => wrong.change.seq += 1,
            "at" => wrong.at += 1,
            "module" => wrong.change.source.module = "other".into(),
            "kind" => wrong.change.source.kind = "job".into(),
            "object" => wrong.change.source.object = "resolved-job".into(),
            "detail" => wrong.change.detail = b"not wire data".to_vec(),
            _ => unreachable!(),
        }
        let error = match field {
            "detail" => {
                sdk::wire::decode::<::tasks::JobEventDetail>(&wrong.change.detail).unwrap_err()
            }
            _ => "wrong change".into(),
        };
        cases.push((vec![wrong], error));
    }
    for (changes, error) in cases {
        let reply = serde_json::to_value(attribution::AttributionReply::Changes(changes)).unwrap();
        let (rpc, server) = scripted_server(vec![(changes_request(), reply)]).await;
        let mut entry = bell_summary(&row, "Alice");
        assert_eq!(
            bell_source(&rpc, &row, &mut entry).await.unwrap_err(),
            error
        );
        server.await.unwrap();
        assert_eq!(entry.detail, "Task activity");
        assert_eq!(entry.target, BellTarget::Unavailable);
        assert!(entry.object.is_empty());
        assert!(!bell_openable(&row, &[entry]));
    }
}

#[test]
fn bell_account_scope_rejects_a_wallet_switch_before_any_rpc_write() {
    assert!(ensure_bell_account("4", 4).is_ok());
    assert!(ensure_bell_account("4", 5).is_err());
    assert!(ensure_bell_account("", 5).is_err());
}
