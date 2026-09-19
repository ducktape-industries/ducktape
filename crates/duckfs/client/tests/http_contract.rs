//! contract tests for `HttpNode` against a hand-rolled `std::net::TcpListener`
//! stub — the `daemon_e2e.rs` raw-http house style inverted (we are the server).
//!
//! this pins the exact request lines/bodies the engine sends and the reply
//! shapes it parses, WITHOUT a daemon: a stage POSTs raw bytes and reads
//! `{digest}`, a commit POSTs the snake_case body and reads the CAMELCASE
//! `BlockSummary`, and a module rejection arriving as a 400
//! `{"error": ..., "reason": ...}` surfaces as `ApiError::Rejected` with BOTH
//! halves verbatim (the conflict taxonomy depends on the sentence; the screen
//! depends on the class). the real daemon round-trip lives in
//! `bin/noded/tests/daemon_e2e.rs`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_client::api::{ApiError, NodeApi};
use duckfs_client::http::HttpNode;
use duckfs_core::{Change, Content};

/// one captured request: the method, the path (with query), and the raw body.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: Vec<u8>,
    operator_token: Option<String>,
}

/// a canned http server that records every request and answers each with a
/// responder-chosen `(status, json)` keyed on the path. it always closes the
/// connection (one request per socket) so reqwest opens a fresh one each call —
/// the simplest thing a hand-rolled server can promise.
struct Stub {
    addr: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Stub {
    fn new<F>(responder: F) -> Self
    where
        F: Fn(&str, &str, &[u8]) -> (u16, serde_json::Value) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr").to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let reqs = requests.clone();
        let stop = shutdown.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let recorded = read_request(&mut stream);
                reqs.lock().unwrap().push(recorded.clone());
                let (status, body) = responder(&recorded.method, &recorded.path, &recorded.body);
                write_response(&mut stream, status, &body);
            }
        });
        Stub {
            addr,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // nudge the blocking accept so the loop notices the shutdown flag.
        let _ = TcpStream::connect(&self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// read one http/1.1 request: the request line, headers to the blank line, then
/// exactly `content-length` body bytes.
fn read_request(stream: &mut TcpStream) -> Recorded {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stub stream"));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut operator_token = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("header line");
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("x-ducktape-admin-token")
        {
            operator_token = Some(value.trim().to_owned());
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).expect("request body");
    }
    Recorded {
        method,
        path,
        body,
        operator_token,
    }
}

/// write a canned http/1.1 response with an explicit content-length and a close.
fn write_response(stream: &mut TcpStream, status: u16, body: &serde_json::Value) {
    let payload = serde_json::to_vec(body).expect("response body serializes");
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

fn signing_node(stub: &Stub) -> HttpNode {
    HttpNode::new(stub.url()).with_frame_signer(std::sync::Arc::new(|target, payload| {
        assert_eq!(target, "files");
        [b"signed-frame:".as_slice(), payload.as_slice()].concat()
    }))
}

#[test]
fn stage_submits_the_exact_signed_binary_module_op() {
    let stub = Stub::new(|_, _, _| (200, serde_json::json!({ "height": 4 })));
    let node = signing_node(&stub);
    let digest = node.stage_chunk(b"abc").unwrap();
    assert_eq!(
        digest,
        duckfs_core::to_hex(&duckfs_core::objects::object_id(
            duckfs_core::Kind::Chunk,
            b"abc"
        ))
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/submit/frame");
    assert_eq!(
        requests[0].body,
        [
            b"signed-frame:".as_slice(),
            &duckfs_core::encode_putblob(b"abc")
        ]
        .concat()
    );
}

#[test]
fn writes_are_module_messages_inside_opaque_signed_frames() {
    let stub = Stub::new(|_, _, _| (200, serde_json::json!({ "height": 5 })));
    let node = signing_node(&stub);
    let receipt = node
        .commit(
            Some("basesnap"),
            "hello",
            vec![Change::Put {
                path: "/shared/x".into(),
                exec: false,
                meta: Default::default(),
                content: Content::Inline {
                    b64: STANDARD.encode(b"hi"),
                },
            }],
        )
        .unwrap();
    assert_eq!(receipt.height, 5);
    for name in [".", "..", "a/b", "café-🦆"] {
        node.unpin(name).unwrap();
    }
    let requests = stub.requests();
    let decoded = |body: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(body.strip_prefix(b"signed-frame:").unwrap())
            .unwrap()
    };
    let commit = decoded(&requests[0].body);
    assert_eq!(commit["commit"]["base_snapshot"], "basesnap");
    assert_eq!(commit["commit"]["changes"][0]["put"]["path"], "/shared/x");
    for (request, name) in requests[1..].iter().zip([".", "..", "a/b", "café-🦆"]) {
        assert_eq!(request.path, "/v1/submit/frame");
        assert_eq!(decoded(&request.body)["unpin"]["name"], name);
    }
}

#[test]
fn module_rejection_is_preserved_and_missing_signer_sends_nothing() {
    let stub = Stub::new(|_, _, _| {
        (
            400,
            serde_json::json!({
                "error": "conflict: /x changed since base",
                "reason": "files_commit",
            }),
        )
    });
    let unsigned = HttpNode::new(stub.url());
    assert!(unsigned.commit(None, "m", Vec::new()).is_err());
    assert!(stub.requests().is_empty());
    let node = signing_node(&stub);
    let refused = node.commit(None, "m", Vec::new()).unwrap_err();
    assert_eq!(
        refused,
        ApiError::Rejected {
            reason: "files_commit".into(),
            sentence: "conflict: /x changed since base".into(),
        }
    );
    // the commit lane's own error reads the same line the read lane does.
    assert_eq!(
        duckfs_client::commit::CommitError::from(refused).to_string(),
        "conflict: /x changed since base [files_commit]"
    );
}

/// a node nothing answers for is its OWN case, carrying the base it was dialed
/// at — not reqwest's sentence and the url folded into a transport string,
/// which left the CLI nothing to classify (#2660). a hang-up before any
/// response is the same condition: it is what a draining node does to a read.
#[test]
fn nothing_answering_is_unreachable_not_a_transport_string() {
    // bound only to learn a port nothing is on: the connect is REFUSED.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let refused = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);
    assert_eq!(
        HttpNode::new(&refused).ls("/", None, None, 10).unwrap_err(),
        ApiError::Unreachable { base: refused }
    );

    // accept, then hang up without writing a byte.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let draining = format!("http://{}", listener.local_addr().expect("addr"));
    let drain = std::thread::spawn(move || drop(listener.accept().expect("the client connects")));
    let failure = HttpNode::new(&draining).ls("/", None, None, 10).unwrap_err();
    drain.join().expect("the drain thread finishes");
    assert_eq!(failure, ApiError::Unreachable { base: draining });
}

/// BOTH halves of the node's envelope survive the read lane, and the error a
/// person ends up reading is `<sentence> [<reason>]` — the module's own words
/// first, the class token last, and no Rust type name anywhere in between.
#[test]
fn a_read_refusal_keeps_its_class_beside_its_sentence() {
    let stub = Stub::new(|_, _, _| {
        (
            400,
            serde_json::json!({ "error": "files: path not found", "reason": "files_query" }),
        )
    });
    let node = HttpNode::new(stub.url());
    let refused = node.stat("/nope", None).unwrap_err();
    assert_eq!(
        refused,
        ApiError::Rejected {
            reason: "files_query".into(),
            sentence: "files: path not found".into(),
        }
    );
    assert_eq!(refused.to_string(), "files: path not found [files_query]");
    assert!(!refused.to_string().contains("Module("));
}

/// an envelope with no class — the node refusing before any module saw the
/// request — is filed under the unclassified one rather than given an invented
/// token, and its sentence still reaches the reader whole.
#[test]
fn an_unclassified_refusal_is_not_given_a_made_up_class() {
    let stub = Stub::new(|_, _, _| (400, serde_json::json!({ "error": "invalid module target" })));
    let node = HttpNode::new(stub.url());
    assert_eq!(
        node.stat("/x", None).unwrap_err(),
        ApiError::Rejected {
            reason: sdk::refusal::UNFRAMED_REFUSAL.into(),
            sentence: "invalid module target".into(),
        }
    );
}

#[test]
fn reads_use_generic_queries_and_decode_the_guest_reply() {
    let stub = Stub::new(|method, path, body| {
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/query");
        let ask: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(ask["target"], "files");
        let query = ask["query"].as_object().unwrap();
        let (kind, _) = query.iter().next().unwrap();
        let reply = match kind.as_str() {
            "refs" => {
                serde_json::json!({ "refs": { "head": "cd".repeat(32), "pins": {}, "window_len": 3 } })
            }
            "read" => {
                serde_json::json!({ "read": { "b64": STANDARD.encode(b"hello"), "eof": true } })
            }
            "has_chunks" => serde_json::json!({ "has_chunks": { "present": [true, false] } }),
            "stat" => serde_json::json!({ "stat": null }),
            _ => panic!("unexpected query"),
        };
        (200, reply)
    });
    let node = HttpNode::new(stub.url());
    assert_eq!(node.refs().unwrap().window_len, 3);
    assert_eq!(
        node.read("/shared/x", Some("snapshot"), 4, 1024).unwrap(),
        (b"hello".to_vec(), true)
    );
    assert_eq!(
        node.has_chunks(&["aa".repeat(32), "bb".repeat(32)])
            .unwrap(),
        vec![true, false]
    );
    assert_eq!(node.stat("/shared/missing", None).unwrap(), None);
    let requests = stub.requests();
    let read: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(read["query"]["read"]["offset"], 4);
    assert_eq!(read["query"]["read"]["snapshot"], "snapshot");
    let chunks: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(chunks["query"]["has_chunks"]["ids"][1], "bb".repeat(32));
}

#[test]
fn operator_stage_uses_generic_raw_transport_and_refreshes_credential() {
    let stub = Stub::new(|method, path, body| {
        assert_eq!((method, path), ("POST", "/v1/submit/raw/files"));
        assert_eq!(body, duckfs_core::encode_putblob(b"chunk"));
        (200, serde_json::json!({"height":1}))
    });
    let credential = Arc::new(Mutex::new(Some("first".to_owned())));
    let read = credential.clone();
    let node = HttpNode::new(stub.url())
        .with_operator_credential(Arc::new(move || read.lock().unwrap().clone()));
    node.stage_chunk(b"chunk").unwrap();
    *credential.lock().unwrap() = Some("second".into());
    node.stage_chunk(b"chunk").unwrap();
    *credential.lock().unwrap() = None;
    assert!(node.stage_chunk(b"chunk").is_err());
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].operator_token.as_deref(), Some("first"));
    assert_eq!(requests[1].operator_token.as_deref(), Some("second"));
}
