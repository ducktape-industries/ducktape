use ::chat;
use ::chat::client::{CHAT_HOT_WINDOW_LIMIT, chat_message, mark_message_groups};
use ::chat::index::MsgRow;
use ::node;

use commonware_cryptography::{Signer as _, ed25519};
use futures::StreamExt as _;

use super::*;
use crate::ShellTab;

mod docs;

/// The delivery verdict refuses a body written in another box — the same
/// room on another network, or another room — beside its live gates.
#[test]
fn a_submit_from_another_box_is_refused_at_delivery() {
    let here = composer_scope("http://node", "general");
    let verdict = |scope: &str| {
        submit_verdict(
            false,
            true,
            "general".into(),
            String::new(),
            true,
            scope.into(),
            here.clone(),
        )
    };
    assert!(matches!(verdict(&here), crate::SubmitVerdict::Admitted));
    assert!(matches!(
        verdict(&composer_scope("http://other", "general")),
        crate::SubmitVerdict::Refused
    ));
    assert!(matches!(
        verdict(&composer_scope("http://node", "ops")),
        crate::SubmitVerdict::Refused
    ));
}
mod messages;
mod repos;
mod shell;
mod status;
mod wire;

/// A node that serves `GET /v1/status` EXACTLY ONCE and answers `500` to every
/// later ask for it. `/v1/peers` answers every time — the pin is on the status
/// document, not on the peer sample.
///
/// This is the whole point of the fixture: a loader that reads the chain twice
/// to fill one card cannot get away with it here, whichever field it takes from
/// whichever read. Counting reads is the only pin that survives a rename —
/// #1017's first round asserted identifier names instead, and a reviewer put
/// the literal second `client.status()` back with every name intact and all
/// 272 tests still green.
async fn node_that_serves_its_status_once(status_body: &'static str) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub node");
    let origin = format!("http://{}", listener.local_addr().expect("stub address"));
    let status_reads = std::sync::Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            // Both routes are bodyless GETs, so the request is in hand as soon
            // as the head is.
            while let Ok(read) = stream.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let asked_for_status = String::from_utf8_lossy(&request).contains("/v1/status");
            let already_served =
                asked_for_status && status_reads.fetch_add(1, Ordering::SeqCst) > 0;
            let (code, body) = match (asked_for_status, already_served) {
                (true, false) => ("200 OK", status_body),
                (true, true) => (
                    "500 Internal Server Error",
                    "this node answers /v1/status once",
                ),
                (false, _) => ("200 OK", r#"{"peers":[]}"#),
            };
            let response = format!(
                "HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    origin
}

/// The next update that says something happened.
///
/// THE TIP ARRIVES FIRST, EVERY BLOCK. The node sends the heartbeat on the
/// block wake and only THEN catches its topics up (`crates/noded/src/stream.rs`,
/// the `block_rx` arm), so the head for block N is on the wire before N's ops
/// are. A test that asserts on `live.next()` directly is asserting on that
/// heartbeat, not on its own submit.
async fn next_change(live: &mut futures::stream::BoxStream<'static, LiveUpdate>) -> LiveUpdate {
    loop {
        let update = live.next().await.expect("live stream ended");
        if update.kind != crate::LiveKind::Tip {
            return update;
        }
        // AND WHILE WE ARE HOLDING A REAL ONE, PIN IT HERE. The unit test below
        // builds its own tip, so it can only speak for `live_update` — it stays
        // green if the stream's arm starts asking for a load. These are the
        // tips the node actually sent, decoded by the real client, so this is
        // the assertion that binds the arm.
        assert!(update.height > 0, "a tip carries the head it was sent with");
        assert!(
            !update.load_chat,
            "a tip must not trigger a load — that is a 1 Hz poll on an idle chain"
        );
    }
}

/// drain the live event stream until the index has folded the block at
/// `min_height` — the system's own commit signal, never a timed poll.
async fn wait_for_block(
    live: &mut futures::stream::BoxStream<'static, LiveUpdate>,
    min_height: i64,
) {
    loop {
        let update = live.next().await.expect("live stream ended");
        let folded = matches!(update.kind, crate::LiveKind::Chat | crate::LiveKind::Plane);
        if folded && update.height >= min_height {
            return;
        }
    }
}

async fn submit_test(
    rpc: &RpcClient,
    signer: &ed25519::PrivateKey,
    sequence: u64,
    target: &str,
    payload: Vec<u8>,
) {
    let frame = node::encode_frame(
        signer,
        sequence,
        &sdk::Msg {
            target: target.into(),
            payload,
        },
    );
    rpc.submit_frame(frame).await.unwrap();
}

/// One commit in `repo` holding exactly `files`, on top of `parent`.
fn mirror_commit(
    repo: &git2::Repository,
    parent: Option<git2::Oid>,
    files: &[(&str, &str)],
) -> git2::Oid {
    let mut tree = repo.treebuilder(None).unwrap();
    for (path, contents) in files {
        let blob = repo.blob(contents.as_bytes()).unwrap();
        tree.insert(path, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("mule", "mule@localhost").unwrap();
    let parents: Vec<git2::Commit> = parent
        .map(|oid| vec![repo.find_commit(oid).unwrap()])
        .unwrap_or_default();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &signature, &signature, "mule", &tree, &parent_refs)
        .unwrap()
}

/// A node whose page SEARCH answers and whose page LIST refuses — the exact
/// split the title join has to survive. Answers one request per connection and
/// closes, so the two views of a search never share a socket. Returns its
/// origin.
async fn node_with_a_broken_page_list() -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// The request is in hand once the body reaches its declared length.
    fn request_is_complete(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let declared = head
            .to_lowercase()
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        body.len() >= declared
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub node");
    let origin = format!("http://{}", listener.local_addr().expect("stub address"));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            while let Ok(read) = stream.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request_is_complete(&request) {
                    break;
                }
            }
            // Both views POST to `/v1/index/pages/view`; only the body says
            // which one this is. Shapes copied from the live demo node.
            let asked_for_the_index = String::from_utf8_lossy(&request).contains("list_pages");
            let (status, body) = match asked_for_the_index {
                true => ("500 Internal Server Error", "pages index unavailable"),
                false => (
                    "200 OK",
                    r#"{"hits":[{"block_id":"block-1","author":"system","page_id":"page-1","parent":"page-1","kind":"paragraph","text":"Tail paragraph after the list","height":1,"time":1}]}"#,
                ),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    origin
}

/// One function's body out of a backend module: from its declaration to the
/// first closing brace at column zero, which in fmt'd Rust is its own. Sliced
/// rather than scanned to the next `pub` — `pub(crate)` does not start with
/// `pub `, so that boundary silently runs a negative assertion over the rest
/// of the file and fails on some LATER function's read.
fn backend_fn<'a>(source: &'a str, declaration: &str) -> &'a str {
    source
        .split(declaration)
        .nth(1)
        .unwrap_or_else(|| panic!("{declaration} is declared"))
        .split("\n}\n")
        .next()
        .unwrap_or_else(|| panic!("{declaration} body"))
}

/// A stub node whose `/v1/index/pages/view` replies carry a SCRIPTED fold
/// watermark: one entry per request, the last one repeating forever. Returns
/// its origin plus the live request count, which is how the bound on
/// `await_fold` is observed rather than assumed.
///
/// One request per connection, then close — the same discipline as
/// `node_with_a_broken_page_list`, so no probe can share a socket with the
/// next and read a stale header off it.
async fn node_scripting_its_fold_watermark(
    script: Vec<Option<&'static str>>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let served = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub node");
    let origin = format!("http://{}", listener.local_addr().expect("stub address"));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            while let Ok(read) = stream.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if String::from_utf8_lossy(&request).contains("\r\n\r\n") {
                    break;
                }
            }
            let nth = counter.fetch_add(1, Ordering::SeqCst);
            let folded = script[nth.min(script.len() - 1)];
            let body = r#"{"threads":[]}"#;
            let watermark = folded
                .map(|tip| format!("x-ducktape-folded: {tip}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{watermark}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (origin, served)
}

// Test fixture reads: production timelines are owned by WASM guests.
async fn query_roots(
    rpc: &RpcClient,
    channel_id: &str,
    before_seq: Option<u64>,
) -> Result<Vec<MsgRow>, String> {
    let reply: ChatViewReply = rpc
        .view(
            "chat",
            &ChatViewQuery::Roots {
                channel_id: channel_id.to_string(),
                before_seq,
                limit: Some(CHAT_HOT_WINDOW_LIMIT),
            },
        )
        .await?;
    let ChatViewReply::Roots {
        roots,
        has_more,
        next_before_seq,
    } = reply
    else {
        return Err("node returned an invalid root page".into());
    };
    let expected_cursor = if has_more {
        roots.first().map(|row| row.seq)
    } else {
        None
    };
    let roots_are_strictly_ordered = roots.windows(2).all(|pair| pair[0].seq < pair[1].seq);
    let roots_precede_request =
        before_seq.is_none_or(|before| roots.iter().all(|row| row.seq < before));
    let roots_are_timeline_rows = roots.iter().all(|row| row.thread.is_none());
    let page_has_a_cursor_source = !has_more || !roots.is_empty();
    let cursor_is_valid = next_before_seq == expected_cursor;
    if !roots_are_strictly_ordered
        || !roots_precede_request
        || !roots_are_timeline_rows
        || !page_has_a_cursor_source
        || !cursor_is_valid
    {
        return Err("node returned an invalid root cursor".into());
    }
    Ok(roots)
}

async fn load_messages(rpc: &RpcClient, channel_id: &str) -> Result<Vec<ChatMessage>, String> {
    let roots = query_roots(rpc, channel_id, None).await?;
    let facts = ReaderFacts::current().await;
    let mut messages: Vec<ChatMessage> = roots
        .into_iter()
        .map(|row| chat_message(row, facts.reader()))
        .collect();
    mark_message_groups(&mut messages);
    Ok(messages)
}
