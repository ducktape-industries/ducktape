//! Process-level gateway proof over two real, TUN-less WireGuard nodes.
//!
//! Alice publishes `api.alice.duck` to one loopback HTTP server. Bob resolves
//! the finalized route, sends POST and browser traffic over the authenticated
//! userspace WireGuard stream, and is identified to Alice by NODE only — a
//! caller account is stamped solely from a user proof-of-possession, which a
//! peer node's proxy request does not carry.
//! The same live cluster proves that stale revisions, undeclared methods,
//! ambient credentials, cross-origin browser calls, and owner-only policy fail
//! before reaching loopback.

mod common;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use common::{Cluster, create_account, hex, submit_frame};
use commonware_cryptography::{Signer as _, ed25519};
use gateway::{
    DuckDnsName, GatewayMsg, GatewayQuery, GatewayReply, MemberAuthorization, RouteAudience,
    RouteDefinition, RouteMethod, RouteName, RoutePolicy, RouteStatement, RouteTarget,
};

const READY: Duration = Duration::from_secs(180);
const FINALIZE: Duration = Duration::from_secs(60);

fn resolve_alice(cluster: &Cluster, reader: usize) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Resolve {
            name: DuckDnsName {
                handle: "alice".into(),
            },
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Resolved(Some(account)) => Some(account.account_id),
        _ => None,
    }
}

fn signed_route(
    member: &ed25519::PrivateKey,
    chain: &str,
    account: u64,
    publisher: &[u8],
    revision: u64,
    audience: RouteAudience,
) -> GatewayMsg {
    let statement = RouteStatement {
        chain_id: chain.into(),
        account_id: account,
        name: RouteName::named("api"),
        publisher_node: publisher.to_vec(),
        revision,
        route: Some(RouteDefinition {
            target: RouteTarget::LoopbackHttp,
            policy: RoutePolicy {
                audience,
                methods: vec![RouteMethod::Get, RouteMethod::Head, RouteMethod::Post],
                max_request_bytes: Some(1024),
                max_response_bytes: 4096,
                allow_authorization: false,
                allow_upgrade: false,
            },
        }),
    };
    let signature = member
        .sign(
            gateway::GATEWAY_ROUTE_NS,
            &gateway::route_signing_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::SetRoute {
        statement,
        authorization: MemberAuthorization {
            signer: member.public_key().as_ref().to_vec(),
            signature,
        },
    }
}

fn route_revision(cluster: &Cluster, reader: usize, account: u64) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Get {
            account_id: account,
            name: RouteName::named("api"),
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Route(record) => record
            .as_ref()
            .as_ref()
            .map(|record| record.statement.revision),
        _ => None,
    }
}

/// the zero-length chunk that ends a chunk-framed body.
const CHUNKED_END: &str = "0\r\n\r\n";

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let count = stream.read(&mut chunk).expect("read loopback request");
        assert!(count > 0, "loopback request closed before its body");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end + 4]);
        // A body the gateway forwarded has no declared length — the head it
        // was carried in no longer states one — so it arrives CHUNK FRAMED and
        // the terminating zero-length chunk is what says it is whole.
        let chunked = headers
            .lines()
            .any(|line| line.to_ascii_lowercase().trim() == "transfer-encoding: chunked");
        if chunked {
            if request.ends_with(CHUNKED_END.as_bytes()) {
                return request;
            }
            continue;
        }
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            return request;
        }
    }
}

/// Alice's loopback upstream, asserting what the gateway stamps on each hop:
/// the vouched-for caller NODE, the route's account NUMBER — and NO caller
/// account: a `/v1/gateway/proxy` request from a peer node carries no user
/// proof-of-possession, and a caller without one has no account.
fn spawn_loopback(
    alice_node: Vec<u8>,
    bob_node: Vec<u8>,
    alice_account: u64,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Alice loopback");
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        for expected in ["POST /items HTTP/1.1", "GET /page HTTP/1.1"] {
            let (mut stream, _) = listener.accept().expect("accept gateway proxy");
            let raw = read_http_request(&mut stream);
            let text = String::from_utf8_lossy(&raw);
            assert!(
                text.starts_with(expected),
                "unexpected upstream request:\n{text}"
            );
            let lower = text.to_ascii_lowercase();
            assert!(
                !lower.contains("x-duck-caller-account"),
                "no user PoP, no caller account:\n{text}"
            );
            assert!(lower.contains(&format!("x-duck-caller-node: {}", hex(&bob_node))));
            assert!(lower.contains(&format!("x-duck-route-account: {alice_account}\r\n")));
            assert!(lower.contains("x-duck-route-label: api"));
            assert!(!lower.contains("cookie:"));
            assert!(!lower.contains("authorization:"));
            assert!(!lower.contains("x-forwarded-"));
            assert_ne!(alice_node, bob_node);
            if expected.starts_with("POST") {
                assert!(
                    text.contains("{\"name\":\"duck\"}") && text.ends_with(CHUNKED_END),
                    "the body arrives chunk framed and whole:\n{text}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nSet-Cookie: secret=nope\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                    )
                    .unwrap();
            } else {
                let body = b"<!doctype html><title>Alice</title><script>document.body.dataset.ok='yes'</script>";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: secret=nope\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        }
    });
    (port, handle)
}

fn proxy_request(
    cluster: &Cluster,
    account: u64,
    revision: u64,
    method: &str,
    path: &str,
    headers: serde_json::Value,
    body: &[u8],
) -> (u16, serde_json::Value) {
    cluster.http(
        1,
        "POST",
        "/v1/gateway/proxy",
        Some(&serde_json::json!({
            "head": {
                "account_id": account,
                "name": { "label": "api" },
                "revision": revision,
                "method": method,
                "path_and_query": path,
                "headers": headers,
                // `ProxyRequestHead` is `deny_unknown_fields` AND has no
                // `serde(default)` on `upgrade`, so omitting it is not a
                // permissive miss — the whole head fails to deserialize and the
                // handler answers 422 with an empty body.
                "upgrade": false,
            },
            "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
        })),
    )
}

fn raw_browser_request(port: u16, authority: &str, extra: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect browser gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    // The duck:// scheme handler forwards the page's authority and origin; there
    // is no <token>.localhost Host any more.
    let request = format!(
        "GET /page HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Duck-Authority: {authority}\r\nOrigin: duck://{authority}\r\nSec-Fetch-Site: same-origin\r\n{extra}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("browser response headers");
    let headers = String::from_utf8_lossy(&raw[..split + 4]).into_owned();
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (status, headers, raw[split + 4..].to_vec())
}

#[test]
fn gateway_runs_over_inline_wireguard_and_fails_closed() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.wireguard = true;
    for index in 0..2 {
        cluster.spawn(index);
    }
    for index in 0..2 {
        cluster.wait_marker(index, "rpc listening on", READY);
        cluster.wait_marker(index, "converged root_hash=", READY);
        cluster.wait_marker(index, "peer handshake COMPLETE", READY);
        cluster.wait_marker(index, "gateway plane: overlay stream bound", READY);
    }

    // Alice founds her account through her node; Bob needs none — his node
    // is a caller, and a node is never an account.
    let alice = ed25519::PrivateKey::from_seed(42);
    let alice_node = Cluster::identity(0);
    let bob_node = Cluster::identity(1);
    let alice_account = create_account(&cluster, 0, &alice, "alice");

    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&GatewayMsg::SetHandle {
            handle: Some("alice".into()),
        }),
    );
    for reader in 0..2 {
        let resolved = cluster.await_committed(reader, "alice.duck resolution", FINALIZE, || {
            resolve_alice(&cluster, reader)
        });
        assert_eq!(resolved, alice_account);
    }

    let (loopback_port, upstream) =
        spawn_loopback(alice_node.clone(), bob_node.clone(), alice_account);
    let workspace = cluster.workspace(0);
    let (ok, output) = cluster.run_verb(&[
        "gateway",
        "bind",
        "--trusted-loopback",
        "--workspace",
        workspace.to_str().unwrap(),
        "--label",
        "api",
        "--port",
        &loopback_port.to_string(),
        // whose signed route this node agrees to serve under that label —
        // without a node dial for the active wallet's account, which a test
        // harness has no business inheriting from the host keystore.
        "--account",
        &alice_account.to_string(),
    ]);
    assert!(ok, "local gateway bind failed: {output}");

    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            1,
            RouteAudience::Network,
        )),
    );
    cluster.await_committed(1, "gateway route revision 1", FINALIZE, || {
        (route_revision(&cluster, 1, alice_account) == Some(1)).then_some(())
    });

    let body = br#"{"name":"duck"}"#;
    let (status, response) = proxy_request(
        &cluster,
        alice_account,
        1,
        "post",
        "/items",
        serde_json::json!([{ "name": "content-type", "value": "application/json" }]),
        body,
    );
    assert_eq!(status, 200, "gateway POST failed: {response}");
    assert_eq!(response["head"]["status"], 201);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(response["body_b64"].as_str().unwrap())
            .unwrap(),
        br#"{"ok":true}"#
    );
    // Set-Cookie is forwarded end to end.
    assert!(
        response["head"]["headers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|header| header["name"] == "set-cookie")
    );

    // Discover the dedicated browser-gateway port, then browse api.alice.duck
    // exactly as the duck:// scheme handler would: the node resolves the
    // authority (duckdns + route) fresh, with no session token.
    let (status, browser) = cluster.http(1, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let browser_port: u16 = browser["base"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let authority = "api.alice.duck";
    let (status, headers, html) = raw_browser_request(browser_port, authority, "");
    assert_eq!(status, 200, "browser gateway failed: {headers}");
    let lower_headers = headers.to_ascii_lowercase();
    assert!(lower_headers.contains("content-security-policy:"));
    assert!(lower_headers.contains(&format!("connect-src duck://{authority}")));
    assert!(lower_headers.contains("worker-src 'none'"));
    assert!(lower_headers.contains("webrtc 'block'"));
    // the browser lane forwards Set-Cookie like the programmatic lane above:
    // the pane's document origin is duck://<authority> (see the CSP asserts),
    // so the embedded browser scopes cookies per route authority exactly like
    // the web — and a self-hosted app behind a route needs its session cookie.
    assert!(lower_headers.contains("set-cookie:"));
    assert!(String::from_utf8_lossy(&html).contains("<title>Alice</title>"));

    // A page whose Origin does not match the forwarded authority is rejected.
    let mut cross = TcpStream::connect(("127.0.0.1", browser_port)).unwrap();
    let request = format!(
        "GET /page HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Duck-Authority: {authority}\r\nOrigin: duck://evil.alice.duck\r\nConnection: close\r\n\r\n"
    );
    cross.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    cross.read_to_string(&mut raw).unwrap();
    assert!(
        raw.starts_with("HTTP/1.1 403"),
        "cross-origin response: {raw}"
    );
    upstream.join().unwrap();

    for (method, path, headers, expected) in [
        ("delete", "/items", serde_json::json!([]), 403),
        // Cookie flows to the upstream.
        (
            "get",
            "/items",
            serde_json::json!([{ "name": "authorization", "value": "Bearer secret" }]),
            403,
        ),
        ("get", "http://127.0.0.1:9/", serde_json::json!([]), 400),
    ] {
        let (status, _) = proxy_request(&cluster, alice_account, 1, method, path, headers, &[]);
        assert_eq!(
            status, expected,
            "unexpected policy result for {method} {path}"
        );
    }

    // owner-only: a caller with no user PoP has no account, so it is never the
    // owner — the peer node's request is refused before reaching loopback.
    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            2,
            RouteAudience::Owner,
        )),
    );
    cluster.await_committed(1, "gateway route revision 2", FINALIZE, || {
        (route_revision(&cluster, 1, alice_account) == Some(2)).then_some(())
    });
    let (status, _) = proxy_request(
        &cluster,
        alice_account,
        1,
        "get",
        "/items",
        serde_json::json!([]),
        &[],
    );
    assert_eq!(status, 409, "stale revision must conflict");
    let (status, response) = proxy_request(
        &cluster,
        alice_account,
        2,
        "get",
        "/items",
        serde_json::json!([]),
        &[],
    );
    assert_eq!(status, 403, "owner-only audience leaked: {response}");
}
