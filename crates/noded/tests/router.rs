//! router contract tests over a FAKE node actor: the http layer must forward
//! commands, translate replies, and map failures — without a live host. the
//! real actor is exercised end-to-end by running the binary.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use commonware_cryptography::Signer as _;
use futures::StreamExt as _;
use futures::channel::mpsc;
use http_body_util::BodyExt as _;
use noded::{
    AdminConfig, AdminExposure, BlockSummary, ModuleCategory, ModuleStatus, NodeCommand,
    NodeHandle, NodeStatus,
};
use tower::ServiceExt as _;

/// a scripted actor: answers every command the same way, like a module host
/// that always succeeds (or always fails, for the error-path tests).
fn spawn_fake_actor(mut cmds: mpsc::Receiver<NodeCommand>, submit_err: Option<&'static str>) {
    tokio::spawn(async move {
        while let Some(cmd) = cmds.next().await {
            match cmd {
                NodeCommand::Submit {
                    target,
                    payload,
                    required_blob: _,
                    origin,
                    reply,
                } => {
                    let result = match submit_err {
                        Some(err) => Err(err.to_string()),
                        None => {
                            // echo enough back to prove the request crossed intact.
                            // the wire casing is the interface crates' serde
                            // convention: snake_case variants and fields.
                            assert_eq!(target, "chat");
                            let value: serde_json::Value =
                                serde_json::from_slice(&payload).expect("payload is json");
                            assert_eq!(value["create_channel"]["channel_id"], "general");
                            // the block reply doubles as the origin probe: echo
                            // the stamped origin so tests assert per-request
                            // identity without a second channel.
                            Ok(BlockSummary {
                                height: 7,
                                root_hash: String::from_utf8_lossy(&origin).into_owned(),
                            })
                        }
                    };
                    let _ = reply.send(result);
                }
                // the signed-frame lane, answered as BOTH real binaries answer
                // it: the origin is the frame's VERIFIED signer, never a caller
                // string. echoed back through root_hash — the same origin probe
                // the frameless arm above uses.
                NodeCommand::SubmitFrame { frame, reply } => {
                    let result = match node::decode_frame(&frame) {
                        Ok((sdk::Origin::External(key), msg)) => {
                            assert_eq!(msg.target, "chat");
                            Ok(BlockSummary {
                                height: 11,
                                root_hash: noded::hex_bytes(&key),
                            })
                        }
                        Ok((origin, ..)) => Err(format!("a frame cannot carry {origin:?}")),
                        Err(err) => Err(err.to_string()),
                    };
                    let _ = reply.send(result);
                }
                NodeCommand::Query { target, req, reply } => {
                    assert_eq!(target, "tasks");
                    let value: serde_json::Value =
                        serde_json::from_slice(&req).expect("query is json");
                    assert_eq!(value, serde_json::json!("list"));
                    let _ = reply.send(Ok(
                        serde_json::to_vec(&serde_json::json!({ "tasks": [] })).unwrap()
                    ));
                }
                // the AUTHENTICATED read lane, answered as the real actors
                // answer it: `reader` becomes the module's `Origin::External`.
                // Echoed back beside the request so a test can assert WHICH key
                // the module was told is asking — the one thing this lane
                // exists to establish.
                NodeCommand::QueryAs {
                    target,
                    req,
                    reader,
                    reply,
                } => {
                    let query: serde_json::Value =
                        serde_json::from_slice(&req).expect("query is json");
                    let _ = reply.send(Ok(serde_json::to_vec(&serde_json::json!({
                        "target": target,
                        "reader": noded::hex_bytes(&reader),
                        "query": query,
                    }))
                    .unwrap()));
                }
            }
        }
    });
}

/// the fake node's own consensus key — every real serve path carries one
/// ([`bin/node`'s `operator_wallet_key`] wiring), so a test node modeling one
/// must too: a keyless node now refuses to verify ANY signature at all
/// ([`WriteRefusal::NodeUnidentified`]), which would make a signed-request
/// test here indistinguishable from that refusal instead of exercising it.
const NODE_KEY: [u8; 32] = [0x11; 32];

/// a fake node that carries an operator credential, the way every real serve
/// path does ([`AdminConfig::minted`]). The mutating routes admit EITHER a user
/// signature or that credential, so a test node without one could not model a
/// local daemon's write at all.
fn local_node() -> (NodeHandle, mpsc::Receiver<NodeCommand>, noded::StreamHub) {
    let (handle, cmd_rx, events) = NodeHandle::channel();
    let handle = handle.with_admin(AdminConfig {
        operator_token: Some(OPERATOR.to_string()),
        node_key: Some(NODE_KEY.to_vec()),
        ..Default::default()
    });
    (handle, cmd_rx, events)
}

/// one request from a LOCAL DAEMON: the operator credential and a loopback
/// peer, which is what `into_make_service_with_connect_info` puts there on a
/// real node. `/v1/submit` and the duckfs writes are its lane.
fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    with_operator(with_peer(
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        "127.0.0.1:40000",
    ))
}

/// the acting key a test signs as. ANY ed25519 key may act — the gate proves
/// possession, not membership — so a test mints one exactly like a client.
fn caller() -> commonware_cryptography::ed25519::PrivateKey {
    commonware_cryptography::ed25519::PrivateKey::from_seed(77)
}

/// one SIGNED mutating request, built the way every in-tree client builds one:
/// through `noded::signed_req::request_headers`, never a hand-rolled trio.
/// salted against `local_node()`'s [`NODE_KEY`], the node these requests are
/// always aimed at in this file.
fn signed(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    signed_by(&caller(), method, uri, body)
}

/// [`signed`], but with an explicit signer — for a test that needs to tell two
/// different acting keys apart (e.g. a pin's owner vs. anyone else).
fn signed_by(
    signer: &commonware_cryptography::ed25519::PrivateKey,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> Request<Body> {
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in noded::signed_req::request_headers(signer, method, uri, &NODE_KEY, &bytes)
    {
        req = req.header(name, value);
    }
    req.body(Body::from(bytes)).unwrap()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response body is json")
}

/// A job's request body, assembled — what a publisher lane that buffers by
/// contract does with the frames a streamed request arrives as.
async fn request_body(mut body: noded::GatewayRequestBody) -> Vec<u8> {
    let mut carried = Vec::new();
    while let Some(chunk) = body.recv().await {
        carried.extend_from_slice(&chunk.expect("the request body was refused"));
    }
    carried
}

#[tokio::test]
async fn submit_forwards_the_payload_and_returns_the_block() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({
                "target": "chat",
                "payload": { "create_channel": { "channel_id": "general", "name": "General" } },
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["height"], 7);
    // the fake actor echoes the stamped origin here — no origin sent, so the
    // daemon default applies
    assert_eq!(body["root_hash"], "noded");
}

#[tokio::test]
async fn submit_stamps_the_client_origin() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({
                "target": "chat",
                "payload": { "create_channel": { "channel_id": "general", "name": "General" } },
                "origin": "jess",
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["root_hash"], "jess");
}

#[tokio::test]
async fn raw_submit_preserves_arbitrary_module_bytes_and_node_authority() {
    let (handle, mut commands, _events) = local_node();
    tokio::spawn(async move {
        let NodeCommand::Submit {
            target,
            payload,
            required_blob: _,
            origin,
            reply,
        } = commands.next().await.unwrap()
        else {
            panic!("expected module submit");
        };
        assert_eq!(target, "new-product");
        assert_eq!(payload, vec![0, 255, 123, 0]);
        assert_eq!(origin, noded::DEFAULT_ORIGIN.as_bytes());
        reply
            .send(Ok(BlockSummary {
                height: 8,
                root_hash: "ab".repeat(32),
            }))
            .unwrap();
    });
    let request = with_operator(with_peer(
        Request::builder()
            .method("POST")
            .uri("/v1/submit/raw/new-product")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(vec![0, 255, 123, 0]))
            .unwrap(),
        "127.0.0.1:40000",
    ));
    let response = noded::router(handle).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["height"], 8);
}

#[tokio::test]
async fn operator_submit_accepts_an_explicit_generic_blob_prerequisite() {
    let (handle, mut commands, _events) = local_node();
    let stored = noded::router(handle.clone())
        .oneshot(post("/v1/files/blob", serde_json::json!({"opaque":true})))
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::OK);
    let digest = body_json(stored).await["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let expected_blob = duckfs_core::from_hex_32(&digest).unwrap();
    tokio::spawn(async move {
        let NodeCommand::Submit {
            target,
            payload,
            required_blob,
            reply,
            ..
        } = commands.next().await.unwrap()
        else {
            panic!("expected module submit");
        };
        assert_eq!(target, "unlisted-application");
        assert_eq!(required_blob, Some(expected_blob));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
            serde_json::json!({"publish":true})
        );
        reply
            .send(Ok(BlockSummary {
                height: 9,
                root_hash: "ab".repeat(32),
            }))
            .unwrap();
    });
    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({
                "target":"unlisted-application", "payload":{"publish":true},
                "required_blob":digest,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["height"], 9);
}

#[tokio::test]
async fn operator_submit_rejects_invalid_or_missing_blob_before_actor_dispatch() {
    use futures::FutureExt as _;
    for (digest, expected_status) in [
        (serde_json::Value::Null, StatusCode::UNPROCESSABLE_ENTITY),
        (
            serde_json::json!("AA".repeat(32)),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (serde_json::json!("00"), StatusCode::UNPROCESSABLE_ENTITY),
        (
            serde_json::json!("gg".repeat(32)),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (serde_json::json!("00".repeat(32)), StatusCode::BAD_REQUEST),
    ] {
        let (handle, mut commands, _events) = local_node();
        let response = noded::router(handle.clone()).oneshot(post("/v1/submit", serde_json::json!({
            "target":"unlisted-application", "payload":{"publish":true}, "required_blob":digest,
        }))).await.unwrap();
        assert_eq!(response.status(), expected_status);
        assert!(
            commands.next().now_or_never().is_none(),
            "invalid prerequisite reached actor"
        );
    }
}

#[tokio::test]
async fn raw_submit_preserves_explicit_blob_and_opaque_bytes() {
    let (handle, mut commands, _events) = local_node();
    let stored = noded::router(handle.clone())
        .oneshot(post("/v1/files/blob", serde_json::json!({"raw":true})))
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::OK);
    let digest = body_json(stored).await["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let expected = duckfs_core::from_hex_32(&digest).unwrap();
    let actor = tokio::spawn(async move {
        let NodeCommand::Submit {
            target,
            payload,
            required_blob,
            reply,
            ..
        } = commands.next().await.unwrap()
        else {
            panic!("expected submit");
        };
        assert_eq!(target, "another-product");
        assert_eq!(payload, [0, 255, 0]);
        assert_eq!(required_blob, Some(expected));
        reply
            .send(Ok(BlockSummary {
                height: 10,
                root_hash: "ab".repeat(32),
            }))
            .unwrap();
    });
    let request = with_operator(with_peer(
        Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/submit/raw/another-product?required_blob={digest}"
            ))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(vec![0, 255, 0]))
            .unwrap(),
        "127.0.0.1:40000",
    ));
    let response = noded::router(handle).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    actor.await.unwrap();
}

// ---- the signed-write gate over the mutating routes -------------------------

/// an UNSIGNED mutation is refused before the handler runs — and the refusal
/// names one stable reason, which is what a dashboard counts.
#[tokio::test]
async fn an_unsigned_mutation_is_refused_and_never_reaches_the_actor() {
    let (handle, cmd_rx, _events) = local_node();
    // an actor that would PANIC if a command crossed: the gate must refuse
    // before anything reaches the lane.
    tokio::spawn(async move {
        let mut cmds = cmd_rx;
        if cmds.next().await.is_some() {
            panic!("an unsigned mutation reached the node actor");
        }
    });
    let app = noded::router(handle);

    for (method, uri) in [
        ("POST", "/v1/log-filter"),
        ("POST", "/v1/invite"),
        ("POST", "/v1/submit"),
        ("POST", "/v1/submit/raw/new-product"),
        ("POST", "/v1/fs/workspaces"),
        ("POST", "/v1/fs/workspaces/abc/commit"),
        ("DELETE", "/v1/fs/workspaces/abc"),
        ("POST", "/v1/files/blob"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an unsigned request"
        );
        let body = body_json(response).await;
        assert_eq!(body["reason"], "signature_missing");
    }
}

/// the node's OWN daemons have no user key, so they present the credential
/// `/v1/admin/*` already asks of a local process — and the acting origin stays
/// the node's own name, because a daemon's write IS the node's.
#[tokio::test]
async fn the_operator_credential_admits_a_daemons_write() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({
                "target": "chat",
                "payload": { "create_channel": { "channel_id": "general", "name": "General" } },
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["root_hash"], "noded");
}

/// the credential is loopback-AND-secret, conjunctively: a remote peer holding
/// a leaked token is still refused, exactly as on `/v1/admin/*`.
#[tokio::test]
async fn the_operator_credential_does_not_travel_off_box() {
    let (handle, cmd_rx, _events) = local_node();
    tokio::spawn(async move {
        let mut cmds = cmd_rx;
        if cmds.next().await.is_some() {
            panic!("an off-box write reached the node actor");
        }
    });

    let request = with_operator(with_peer(
        Request::builder()
            .method("POST")
            .uri("/v1/submit")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        "203.0.113.7:40000",
    ));
    let response = noded::router(handle).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["reason"], "signature_missing");
}

/// `/v1/submit` re-signs the framed op with the NODE's own consensus key
/// (`bin/node`'s ingress/park), so any caller reaching it mints an op as the
/// VALIDATOR — the gate must therefore take only the operator (#1808): a
/// self-minted key is refused before the actor ever sees it, and only the
/// operator's OWN signature (never a claim in the body) names the origin.
#[tokio::test]
async fn submit_refuses_a_self_minted_key_and_the_operators_signature_wins_over_the_claim() {
    let operator_key = commonware_cryptography::ed25519::PrivateKey::from_seed(9009);
    let node = || {
        let (handle, cmd_rx, events) = NodeHandle::channel();
        let handle = handle.with_admin(AdminConfig {
            operator_token: Some(OPERATOR.to_string()),
            owner_key: Some(operator_key.public_key().as_ref().to_vec()),
            node_key: Some(NODE_KEY.to_vec()),
            ..Default::default()
        });
        (handle, cmd_rx, events)
    };
    let submit_body = serde_json::json!({
        "target": "chat",
        "payload": { "create_channel": { "channel_id": "general", "name": "General" } },
        "origin": "somebody-else",
    });
    let sign_as = |signer: &commonware_cryptography::ed25519::PrivateKey,
                   body: &serde_json::Value| {
        let bytes = serde_json::to_vec(body).unwrap();
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/submit")
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in
            noded::signed_req::request_headers(signer, "POST", "/v1/submit", &NODE_KEY, &bytes)
        {
            req = req.header(name, value);
        }
        req.body(Body::from(bytes)).unwrap()
    };

    // a key nobody knows, signed correctly: refused before the actor runs.
    let (handle, cmd_rx, _events) = node();
    tokio::spawn(async move {
        let mut cmds = cmd_rx;
        if cmds.next().await.is_some() {
            panic!("a self-minted submit reached the node actor");
        }
    });
    let response = noded::router(handle)
        .oneshot(sign_as(&caller(), &submit_body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["reason"], "not_operator");

    // the operator's own key: admitted, and it names the origin — never the
    // body's `origin` claim (#1312).
    let (handle, cmd_rx, _events) = node();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        .oneshot(sign_as(&operator_key, &submit_body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stamped = body_json(response).await["root_hash"]
        .as_str()
        .expect("origin echo")
        .to_string();
    let acting = String::from_utf8_lossy(operator_key.public_key().as_ref()).into_owned();
    assert_eq!(stamped, acting);
}

/// THE NODE-LEVEL ROUTES, one per credential. `signed_write_guard` proves
/// possession of a SELF-CHOSEN key, which is the right bar only where the key
/// rides on to a module's `check_authority`. These four handlers read no acting
/// identity at all — they mint a year-long mesh invite, retune the process's
/// tracing filter, spawn a host pty, and `remove_dir_all` a managed checkout —
/// so a fresh keypair must buy nothing on any of them.
///
/// The admitted half is the other half of the contract: the operator token and
/// the node's configured operator key BOTH still work, which is what keeps
/// `ducktape node log-filter` (the active wallet key) and the app's invite
/// mint (the token) alive. A route that has nothing wired answers 503/400/204
/// past the gate — the assertion is only that the gate did not stop it.
#[tokio::test]
async fn a_node_level_route_refuses_a_self_minted_key_and_admits_the_operator() {
    // the operator's own wallet key, as `bin/node`'s `operator_wallet_key`
    // reads it out of this host's keystore at boot.
    let operator_key = commonware_cryptography::ed25519::PrivateKey::from_seed(4242);
    let node = || {
        let (handle, cmd_rx, events) = NodeHandle::channel();
        let handle = handle.with_admin(AdminConfig {
            operator_token: Some(OPERATOR.to_string()),
            owner_key: Some(operator_key.public_key().as_ref().to_vec()),
            node_key: Some(NODE_KEY.to_vec()),
            ..Default::default()
        });
        (handle, cmd_rx, events)
    };
    let sign_as = |signer: &commonware_cryptography::ed25519::PrivateKey,
                   method: &str,
                   uri: &str,
                   bytes: Vec<u8>| {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in
            noded::signed_req::request_headers(signer, method, uri, &NODE_KEY, &bytes)
        {
            req = req.header(name, value);
        }
        req.body(Body::from(bytes)).unwrap()
    };

    for (method, uri, body) in [
        ("POST", "/v1/invite", r#"{"ttl_days":365}"#),
        ("POST", "/v1/log-filter", "info"),
        ("DELETE", "/v1/fs/workspaces/abc", ""),
    ] {
        // a key nobody knows, signed correctly. the whole vector.
        let (handle, _cmds, _events) = node();
        let response = noded::router(handle)
            .oneshot(sign_as(&caller(), method, uri, body.as_bytes().to_vec()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a self-minted key"
        );
        assert_eq!(body_json(response).await["reason"], "not_operator");

        // the operator's key, which the node was told about.
        let (handle, _cmds, _events) = node();
        let response = noded::router(handle)
            .oneshot(sign_as(
                &operator_key,
                method,
                uri,
                body.as_bytes().to_vec(),
            ))
            .await
            .unwrap();
        assert!(
            response.status() != StatusCode::FORBIDDEN
                && response.status() != StatusCode::UNAUTHORIZED,
            "{method} {uri} must admit the operator key, got {}",
            response.status()
        );

        // ...and the operator credential from a loopback peer.
        let (handle, _cmds, _events) = node();
        let request = with_operator(with_peer(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
            "127.0.0.1:40000",
        ));
        let response = noded::router(handle).oneshot(request).await.unwrap();
        assert!(
            response.status() != StatusCode::FORBIDDEN
                && response.status() != StatusCode::UNAUTHORIZED,
            "{method} {uri} must admit the operator credential, got {}",
            response.status()
        );
    }
}

/// the OTHER half of the split: a module-bound route still takes any acting
/// key, because the key becomes the op's origin and the module decides. A node
/// that tightened these too would break every duckfs client.
#[tokio::test]
async fn a_module_bound_route_still_admits_any_acting_key() {
    let (handle, _cmds, _events) = local_node();
    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/fs/workspaces",
            serde_json::json!({ "prefix": "/shared/x" }),
        ))
        .await
        .unwrap();
    // no workspace root wired → 503, which is PAST the gate. the point is that
    // an unknown key was not refused.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// a signature that does not bind THIS body is not a signature for THIS
/// request: the gate hashes the body precisely so an authenticated caller's
/// bytes cannot be swapped in flight.
#[tokio::test]
async fn a_swapped_body_defeats_the_signature() {
    let (handle, _cmds, _events) = local_node();
    let app = noded::router(handle);

    let signed_for = serde_json::json!({ "prefix": "/shared/mine" });
    let mut request = signed("POST", "/v1/fs/workspaces", signed_for);
    *request.body_mut() = Body::from(r#"{"prefix":"/shared/yours"}"#);

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["reason"], "signature_invalid");
}

/// reads are NOT gated: the ruling is about mutation, and a status probe with
/// no key must keep working.
#[tokio::test]
async fn reads_stay_open() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let app = noded::router(handle);

    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
}

// ---- the signed-frame lane (`POST /v1/submit/frame`) -----------------------
//
// the lane exists so an op can carry authorship consensus CHECKS instead of a
// caller string it has to trust. these cases pin exactly that: the origin the
// block sees is the frame's verified signer, and a frame whose signature does
// not bind never reaches the actor at all.

/// the raw-bytes request the lane takes — no json envelope, no origin field.
fn post_frame(frame: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/submit/frame")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(frame))
        .unwrap()
}

/// the op every frame case submits (the fake actor asserts the target).
fn chat_op() -> sdk::Msg {
    sdk::Msg {
        target: "chat".into(),
        payload: serde_json::to_vec(
            &serde_json::json!({ "create_channel": { "channel_id": "general", "name": "General" } }),
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn a_signed_frame_lands_with_the_signers_key_as_the_origin() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(42);
    let frame = node::encode_frame(&signer, 1, &chat_op());
    let response = noded::router(handle)
        .oneshot(post_frame(frame))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["height"], 11);
    // the actor echoes the origin it VERIFIED — the signer's public key, which
    // no part of the request could have claimed.
    assert_eq!(
        body["root_hash"],
        noded::hex_bytes(signer.public_key().as_ref())
    );
    // the receipt addresses the op PAYLOAD, exactly as the frameless lane does.
    assert_eq!(
        body["op_hash"].as_str().map(str::len),
        Some(64),
        "the frame lane returns the same receipt shape"
    );
}

#[tokio::test]
async fn signed_blob_prerequisite_requires_local_bytes_and_preserves_the_frame() {
    use futures::FutureExt as _;
    let (handle, mut commands, _events) = local_node();
    let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(42);
    let stored = noded::router(handle.clone())
        .oneshot(post("/v1/files/blob", serde_json::json!({"signed":true})))
        .await
        .unwrap();
    let digest =
        duckfs_core::from_hex_32(body_json(stored).await["digest"].as_str().unwrap()).unwrap();
    let message = sdk::Msg {
        target: "unknown-product".into(),
        payload: vec![255, 0],
    };
    let unavailable = node::encode_frame_with_blob(&signer, 1, &message, Some([0; 32]));
    let response = noded::router(handle.clone())
        .oneshot(post_frame(unavailable))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(commands.next().now_or_never().is_none());
    let frame = node::encode_frame_with_blob(&signer, 2, &message, Some(digest));
    let expected = frame.clone();
    let actor = tokio::spawn(async move {
        let NodeCommand::SubmitFrame { frame, reply } = commands.next().await.unwrap() else {
            panic!("expected signed frame");
        };
        assert_eq!(frame, expected);
        reply
            .send(Ok(BlockSummary {
                height: 12,
                root_hash: "ab".repeat(32),
            }))
            .unwrap();
    });
    let response = noded::router(handle)
        .oneshot(post_frame(frame))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    actor.await.unwrap();
}

#[tokio::test]
async fn a_tampered_frame_is_refused_before_it_reaches_the_actor() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(42);
    let op = chat_op();
    let mut frame = node::encode_frame(&signer, 1, &op);
    // flip one byte of the PAYLOAD: the proof binds (scheme, origin, seq,
    // target, payload), so the frame no longer verifies — and the actor never
    // sees it.
    // located by SEARCHING for the payload rather than counting back from the
    // tail: an offset measured against the frame's trailer silently slides onto
    // a structural byte when the layout gains a field, and the refusal becomes
    // "does not parse" — a different gate than the one under test.
    let at = frame
        .windows(op.payload.len())
        .position(|window| window == op.payload.as_slice())
        .expect("the frame carries the op payload verbatim");
    frame[at] ^= 0x01;

    let response = noded::router(handle)
        .oneshot(post_frame(frame))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    let err = body["error"].as_str().expect("a verbatim refusal");
    assert!(
        err.contains("proof does not bind"),
        "the refusal names the cause: {err}"
    );
}

#[tokio::test]
async fn a_frame_cannot_claim_another_keys_origin() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    // key A signs; the frame is then re-stamped to claim key B's origin — the
    // forgery the whole lane exists to make impossible. the origin is INSIDE the
    // signed preimage, so B's key cannot be swapped in without breaking it.
    let a = commonware_cryptography::ed25519::PrivateKey::from_seed(1);
    let b = commonware_cryptography::ed25519::PrivateKey::from_seed(2);
    let mut frame = node::encode_frame(&a, 1, &chat_op());
    let b_key = b.public_key();
    // the origin is the first length-prefixed field: 8 bytes of length, then the
    // 32 key bytes.
    frame[8..8 + 32].copy_from_slice(b_key.as_ref());

    let response = noded::router(handle)
        .oneshot(post_frame(frame))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a frame signed by A cannot act as B"
    );
}

#[tokio::test]
async fn submit_receipt_op_hash_addresses_the_committed_payload() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let app = noded::router(handle);

    let payload =
        serde_json::json!({ "create_channel": { "channel_id": "general", "name": "General" } });
    let response = app
        .clone()
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({ "target": "chat", "payload": payload }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let op_hash = body["op_hash"].as_str().expect("receipt carries op_hash");
    assert_eq!(op_hash.len(), 64);
    assert!(
        op_hash
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );

    // the hash is ADDRESSABLE, not just informational: the blob lane serves the
    // committed op bytes back under it.
    let fetched = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/files/blob/{op_hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    let bytes = fetched.into_body().collect().await.unwrap().to_bytes();
    let round_trip: serde_json::Value =
        serde_json::from_slice(&bytes).expect("blob is the op json");
    assert_eq!(
        round_trip,
        serde_json::json!({ "create_channel": { "channel_id": "general", "name": "General" } })
    );
}

#[tokio::test]
async fn submit_maps_a_module_error_to_bad_request() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, Some("module error: channel already exists"));

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({ "target": "chat", "payload": {} }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"], "module error: channel already exists");
}

/// The ws surface's policy (`stream.rs` `ClientMsg`) on the HTTP bodies: a
/// field this build does not know is refused BY NAME, never dropped on the
/// floor while the rest of the body is served (#1325). No actor is spawned on
/// purpose — the refusal happens in the extractor, before dispatch.
#[tokio::test]
async fn submit_refuses_an_unknown_field_by_name() {
    let (handle, _cmd_rx, _events) = local_node();

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({ "target": "chat", "payload": {}, "orgin": "jess" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        text.contains("unknown field `orgin`"),
        "the refusal names the field: {text}"
    );
}

#[tokio::test]
async fn query_returns_the_decoded_module_reply() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/query",
            serde_json::json!({ "target": "tasks", "query": "list" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body, serde_json::json!({ "tasks": [] }));
}

// ============================================================================
// /v1/query/reader — the AUTHENTICATED read lane
//
// `/v1/query` proves nothing about its caller, so a module answering it must
// treat every reader as anonymous. This lane is how a module can be told who is
// asking, and these tests pin the only property that makes that worth anything:
// the reader is the VERIFIED signer and nothing a caller wrote.
// ============================================================================

/// the reader identity the fake actor echoes back — what the module was told.
async fn reader_of(response: axum::response::Response) -> String {
    body_json(response).await["reader"]
        .as_str()
        .expect("the actor echoes the reader")
        .to_string()
}

/// an actor that PANICS if any command crosses — the file's idiom for "the gate
/// refused before the handler ran". Deliberately not a `try_next()` probe on the
/// receiver: `oneshot` consumes the router, so by the time a test could poll,
/// the sender may already be dropped and an empty CLOSED channel reads `Ok(None)`
/// rather than the `Err` an empty open one gives. The panic does not care when
/// it runs.
fn spawn_forbidden_actor(cmd_rx: mpsc::Receiver<NodeCommand>) {
    tokio::spawn(async move {
        let mut cmds = cmd_rx;
        if cmds.next().await.is_some() {
            panic!("a refused read reached the node actor");
        }
    });
}

/// no trio, no read. The command lane must never see the request: an
/// unauthenticated caller learns nothing, not even whether the module exists.
#[tokio::test]
async fn an_unsigned_reader_query_is_refused_before_the_actor() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_forbidden_actor(cmd_rx);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/query/reader",
            serde_json::json!({ "target": "collaboration", "query": "mailbox" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(response).await["reason"],
        "signature_missing",
        "the refusal must name why"
    );
    // and the forbidden actor above proves nothing was forwarded.
}

/// a valid trio names the signer, and the module is told exactly that key.
#[tokio::test]
async fn a_signed_reader_query_reaches_the_module_as_its_signer() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/query/reader",
            serde_json::json!({ "target": "collaboration", "query": "mailbox" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        reader_of(response).await,
        noded::hex_bytes(caller().public_key().as_ref()),
        "the module must be told the key that signed"
    );
}

/// THE test this lane exists for.
///
/// A caller supplies request BYTES. If a reader identity could ride in them,
/// every protected read would be forgeable by anyone who can POST the
/// unauthenticated `/v1/query` — which is anything that can dial this port,
/// a sandboxed guest on its vsock tunnel included. So one key signs while the
/// body loudly claims to be another, and the module must hear the signer.
#[tokio::test]
async fn a_reader_named_in_the_body_cannot_displace_the_signer() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let victim = commonware_cryptography::ed25519::PrivateKey::from_seed(9001);
    let victim_hex = noded::hex_bytes(victim.public_key().as_ref());
    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/query/reader",
            serde_json::json!({
                "target": "collaboration",
                // every shape an attacker would try, at both levels.
                "reader": victim_hex,
                "viewer": victim_hex,
                "query": {
                    "messages": { "conversation_id": "c1", "viewer": victim_hex },
                    "reader": victim_hex,
                },
            }),
        ))
        .await
        .unwrap();

    // the envelope's extra top-level keys are simply not part of the request
    // shape, and the query rides through verbatim as the module's own business.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let refusal = body_json(response).await;
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("{target, query}"),
        "an unknown envelope field is refused, not ignored: {refusal}"
    );
}

/// ...and with a WELL-FORMED envelope, a reader claim inside the module's own
/// query never becomes the reader. The module's `Origin` comes from the
/// signature; whatever the query says is the module's own business and cannot
/// reach the identity slot.
#[tokio::test]
async fn a_reader_claim_inside_the_query_is_not_the_reader() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let victim = commonware_cryptography::ed25519::PrivateKey::from_seed(9001);
    let victim_hex = noded::hex_bytes(victim.public_key().as_ref());
    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/query/reader",
            serde_json::json!({
                "target": "collaboration",
                "query": { "mailbox": { "participant_id": "someone-else",
                                        "viewer": victim_hex } },
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["reader"].as_str().unwrap(),
        noded::hex_bytes(caller().public_key().as_ref()),
        "the signer is the reader, whatever the query claims"
    );
    assert_ne!(
        body["reader"].as_str().unwrap(),
        victim_hex,
        "a claimed viewer must never become the reader"
    );
    // the query itself is passed through untouched — the module owns its shape.
    assert_eq!(body["query"]["mailbox"]["participant_id"], "someone-else");
}

/// a signature minted for ANOTHER node does not verify here. The node key is
/// folded into the signed bytes precisely so a captured read cannot be
/// replayed against a different node.
#[tokio::test]
async fn a_reader_query_signed_for_another_node_is_refused() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_forbidden_actor(cmd_rx);

    const OTHER_NODE: [u8; 32] = [0x22; 32];
    let body = serde_json::json!({ "target": "collaboration", "query": "mailbox" });
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/query/reader")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in noded::signed_req::request_headers(
        &caller(),
        "POST",
        "/v1/query/reader",
        &OTHER_NODE,
        &bytes,
    ) {
        req = req.header(name, value);
    }

    let response = noded::router(handle)
        .oneshot(req.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["reason"], "signature_invalid");
}

/// the body is signed, so swapping it after signing is caught. Without this the
/// signature would authenticate a caller while leaving an attacker free to
/// change which conversation was read.
#[tokio::test]
async fn a_reader_query_whose_body_was_swapped_is_refused() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_forbidden_actor(cmd_rx);

    let signed_for = serde_json::json!({ "target": "collaboration", "query": "mine" });
    let sent = serde_json::to_vec(&serde_json::json!({
        "target": "collaboration", "query": "someone-elses"
    }))
    .unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/query/reader")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in noded::signed_req::request_headers(
        &caller(),
        "POST",
        "/v1/query/reader",
        &NODE_KEY,
        &serde_json::to_vec(&signed_for).unwrap(),
    ) {
        req = req.header(name, value);
    }

    let response = noded::router(handle)
        .oneshot(req.body(Body::from(sent)).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["reason"], "signature_invalid");
}

/// the open lane is UNCHANGED and still anonymous. That is not an oversight to
/// be fixed later by a refusal rule here — it is the premise: a module serving
/// protected content refuses `Origin::System`, which is what this lane passes.
#[tokio::test]
async fn the_open_query_lane_stays_open_and_anonymous() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/query",
            serde_json::json!({ "target": "tasks", "query": "list" }),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "adding an authenticated lane must not gate the open one"
    );
}

/// The contract number is a fact about the binary, not the boundary: a node
/// that has published nothing yet serves it too, so the app can refuse (or
/// not) before the first block.
#[tokio::test]
async fn status_carries_the_contract_number_before_any_publish() {
    let (handle, _cmd_rx, _events) = local_node();
    let response = noded::router(handle)
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["contract"], noded::NODE_CONTRACT);
}

#[tokio::test]
async fn status_reports_root_hash_height_and_module_roots() {
    // deliberately NO actor: /v1/status serves the last-published snapshot
    // straight off the handle's cell, so it must answer even when nothing
    // drains the command lane — the wedged-behind-sync regression this cell
    // exists to prevent.
    let (handle, _cmd_rx, _events) = local_node();
    handle.status_cell().publish(NodeStatus {
        contract: noded::NODE_CONTRACT,
        version: "9.9.9".into(),
        root_hash: "cd".repeat(32),
        height: 3,
        consensus_time: 3,
        consensus_time_unit: noded::ConsensusTimeUnit::Height,
        modules: vec![ModuleStatus {
            id: "chat".into(),
            root: "ef".repeat(32),
            category: ModuleCategory::of("chat"),
        }],
        public_key: "ab".repeat(32),
        chain_id: String::new(),
        operations: noded::OperationalStatus {
            role: noded::NodeRole::Validator,
            phase: noded::NodePhase::Validating,
            phase_since: 1_720_000_000,
            ..Default::default()
        },
    });

    let response = noded::router(handle)
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // the app-facing surface's number rides beside the build version: the
    // desktop app reads this one field to decide whether to open a console.
    assert_eq!(body["contract"], noded::NODE_CONTRACT);
    assert_eq!(body["version"], "9.9.9");
    assert_eq!(body["root_hash"], "cd".repeat(32));
    assert_eq!(body["height"], 3);
    assert_eq!(body["consensus_time"], 3);
    assert_eq!(body["consensus_time_unit"], "height");
    assert_eq!(body["modules"][0]["id"], "chat");
    assert_eq!(body["modules"][0]["root"], "ef".repeat(32));
    // the catalog category rides on the wire as a lowercase string.
    assert_eq!(body["modules"][0]["category"], "workspace");
    assert_eq!(body["operations"]["role"], "validator");
    assert_eq!(body["operations"]["phase"], "validating");
    assert_eq!(body["operations"]["phase_since"], 1_720_000_000u64);
}

#[tokio::test]
async fn peers_reports_the_direct_peer_sample() {
    // NO actor: the sample parses from the wired exposition source and the
    // published standing — like status, peers must answer while a sync
    // stage has the pump busy.
    let (handle, _cmd_rx, _events) = local_node();
    let cell = handle.status_cell();
    cell.wire_exposition(|| "network_tracker_directory_connected{peer=\"ab\"} 1000\n".to_string());
    cell.publish_peers(noded::PeersStanding {
        validators: ["ab".to_string()].into(),
        residents: Default::default(),
        height: 41,
        epoch: Some(7),
        builds: [("ab".to_string(), "abc1234".to_string())].into(),
    });

    let response = noded::router(handle)
        .oneshot(
            Request::builder()
                .uri("/v1/peers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["height"], 41);
    assert_eq!(body["epoch"], 7);
    assert_eq!(body["peers"][0]["peer"], "ab");
    assert_eq!(body["peers"][0]["connected"], true);
    assert_eq!(body["peers"][0]["connected_since_ms"], 1000);
    assert_eq!(
        body["peers"][0]["role"], "validator",
        "the published standing stamps roles onto the live sample"
    );
    assert_eq!(
        body["peers"][0]["build"], "abc1234",
        "and the build stamp the peer reported about itself, where one was heard"
    );
}

#[tokio::test]
async fn metrics_forwards_the_encoded_registry_as_openmetrics_text() {
    // NO actor: the scrape reads the wired exposition source directly.
    let (handle, _cmd_rx, _events) = local_node();
    handle.status_cell().wire_exposition(|| {
        "# HELP ducktape_blocks_total blocks\nducktape_blocks_total 3\n# EOF\n".to_string()
    });

    let response = noded::router(handle)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.contains("openmetrics-text"),
        "scrape content type, got {content_type:?}",
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("metrics body is utf-8");
    assert!(
        text.contains("ducktape_blocks_total 3"),
        "the wired exposition passed through: {text:?}"
    );
}

/// this test node's operator credential — what a real node mints 0600 into its
/// workspace and hands only to whoever can read that directory.
const OPERATOR: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// a handle whose admin namespace is gated on [`OPERATOR`], default exposure.
fn operator_handle() -> (NodeHandle, mpsc::Receiver<NodeCommand>) {
    let (handle, cmd_rx, _events) = local_node();
    let handle = handle.with_admin(AdminConfig {
        operator_token: Some(OPERATOR.to_string()),
        ..Default::default()
    });
    (handle, cmd_rx)
}

/// an admin request with the method the route actually serves — `logs/tail` is
/// a GET, and `post()` would answer 405 there long before the gate ran.
fn admin_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

/// stamp the operator credential onto a request the way a client that read
/// `admin.token` out of the node's workspace would.
fn with_operator(mut req: Request<Body>) -> Request<Body> {
    req.headers_mut().insert(
        noded::admin::ADMIN_TOKEN_HEADER,
        OPERATOR.parse().expect("token is a header value"),
    );
    req
}

#[tokio::test]
async fn shutdown_acknowledges_then_signals() {
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let signal = handle.clone();

    // shutdown moved to the owner-gated admin namespace. the default
    // handle has no on-chain owner, so the operator credential is the gate; the
    // loopback check is FAIL-CLOSED on a missing ConnectInfo, so the test stamps
    // a loopback peer exactly as the connect-info make-service would.
    let response = noded::router(handle)
        .oneshot(with_operator(with_peer(
            post("/v1/admin/shutdown", serde_json::json!({})),
            "127.0.0.1:40000",
        )))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["ok"], true);
    // the permit is stored, so awaiting after the request must resolve —
    // guarded by a timeout so a broken signal fails instead of hanging.
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        signal.shutdown_requested(),
    )
    .await
    .expect("shutdown signal fired");
}

/// stamp a peer address onto a request the way `into_make_service_with_connect_info`
/// would, so the admin guard's loopback check has something to read.
fn with_peer(mut req: Request<Body>, addr: &str) -> Request<Body> {
    req.extensions_mut().insert(axum::extract::ConnectInfo(
        addr.parse::<std::net::SocketAddr>()
            .expect("test peer addr"),
    ));
    req
}

/// shutdown left the unauthenticated public surface entirely. the old
/// path is a 404 — flag-day, no alias.
#[tokio::test]
async fn the_old_public_shutdown_route_is_gone() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        .oneshot(post("/v1/shutdown", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "shutdown must not answer on the public surface anymore"
    );
}

/// `DUCKTAPE_ADMIN=off` leaves the control surface simply ABSENT — the admin
/// routes are never registered, so they 404 (not a gated-but-present 403).
#[tokio::test]
async fn a_disabled_admin_namespace_is_absent() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let handle = handle.with_admin(AdminConfig {
        exposure: AdminExposure::Disabled,
        node_key: None,
        ..Default::default()
    });
    let response = noded::router(handle)
        .oneshot(post("/v1/admin/shutdown", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a disabled admin namespace is absent, not forbidden"
    );
}

/// under the default `Loopback` exposure a non-loopback peer is refused before
/// any owner check — the exposure gate is the outer wall.
#[tokio::test]
async fn a_non_loopback_peer_is_refused_under_loopback_exposure() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let request = with_peer(
        post("/v1/admin/shutdown", serde_json::json!({})),
        "203.0.113.7:5555",
    );
    let response = noded::router(handle).oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "loopback-only admin refuses a remote peer"
    );
}

/// The operator trigger for the mid-life netstack backend swap: what the route
/// answers with no plane behind it, with one that takes the swap, with one that
/// refuses it, and on a body it cannot parse.
///
/// A refusal is a 409 and the RUNNING machine continues — the executor's
/// contract — so the route must surface the plane's reason rather than retry.
#[tokio::test]
async fn netstack_swap_answers_the_plane_or_says_there_is_none() {
    // no swapper wired: the node runs no reachability plane, and the route says
    // so instead of pretending a backend exists.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        .oneshot(post(
            "/v1/admin/netstack/swap",
            serde_json::json!({ "backend": { "component": "/srv/working.wasm" } }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(response).await["reason"], "netstack_plane_absent");

    // a wired plane that takes the swap answers the new backend's name; the
    // request the route decoded reaches the plane intact.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    handle.status_cell().wire_netstack_swapper(move |request| {
        recorded.lock().unwrap().push(request.clone());
        Box::pin(async move {
            match request {
                // the frozen snapshot contract refuses a component built
                // against another contract BY NAME, before a byte is decoded.
                noded::NetstackSwapRequest::Component(path) => match path.to_str() {
                    Some("/srv/working.wasm") => Ok("guest".to_string()),
                    _ => Err("foreign snapshot contract".to_string()),
                },
                // the governance reconciler's variant: node-internal, never
                // decoded from a request body (asserted below).
                noded::NetstackSwapRequest::Bytes(_) => Err("bytes are not a route".to_string()),
            }
        })
    });
    let router = noded::router(handle);
    let response = router
        .clone()
        .oneshot(post(
            "/v1/admin/netstack/swap",
            serde_json::json!({ "backend": { "component": "/srv/working.wasm" } }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["backend"], "guest");

    // a refused swap: 409, the plane's own reason in `error`, and the stable
    // token in `reason`.
    let response = router
        .clone()
        .oneshot(post(
            "/v1/admin/netstack/swap",
            serde_json::json!({ "backend": { "component": "/srv/netstack.wasm" } }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["reason"], "netstack_swap_refused");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("foreign snapshot contract"),
        "the plane's refusal must reach the operator: {body}"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            noded::NetstackSwapRequest::Component("/srv/working.wasm".into()),
            noded::NetstackSwapRequest::Component("/srv/netstack.wasm".into()),
        ],
    );

    // a misspelled key must be a 400, never a request that silently swapped
    // nothing — and so must the node-internal bytes variant: this route reads
    // a path on the node's own disk, and nothing off-box ships it component
    // bytes.
    for body in [
        serde_json::json!({ "backend": "native" }),
        serde_json::json!({ "backends": "native" }),
        serde_json::json!({ "backend": { "bytes": [0, 97, 115, 109] } }),
    ] {
        let response = router
            .clone()
            .oneshot(post("/v1/admin/netstack/swap", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["reason"],
            "netstack_swap_body_invalid"
        );
    }
}

/// THE regression this gate exists for: a LOOPBACK process with no operator
/// credential — a service daemon, a stray script, anything that can dial the
/// port — must not be able to stop the node or stage module wasm. The operator,
/// who read `admin.token` out of the node's own workspace, still can.
#[tokio::test]
async fn a_loopback_caller_without_the_operator_credential_cannot_drive_admin() {
    // the two destructive routes AND the one that READS: shutdown stops the
    // process, module-code/stage ingests a wasm artifact and fans it out to
    // members, and logs/tail drains the 4096-line ring — every line the node
    // ever logged, which is a real secret-read surface, not merely a noisy one.
    // Each route with the method it actually serves: a POST to logs/tail is a
    // 405 that would pass this assertion for entirely the wrong reason.
    for (method, route) in [
        ("POST", "/v1/admin/shutdown"),
        ("POST", "/v1/admin/module-code/stage"),
        ("GET", "/v1/admin/logs/tail"),
        // the netstack swap moves the whole reachability plane onto another
        // machine: as much node control as shutdown is.
        ("POST", "/v1/admin/netstack/swap"),
    ] {
        let (handle, cmd_rx) = operator_handle();
        spawn_fake_actor(cmd_rx, None);
        let response = noded::router(handle)
            .oneshot(with_peer(admin_request(method, route), "127.0.0.1:40000"))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{route} must refuse an uncredentialed loopback caller"
        );
        let body = body_json(response).await;
        assert_eq!(body["reason"], "operator_token_missing");
        // the refusal must never hand back the credential it wanted.
        assert!(
            !body.to_string().contains(OPERATOR),
            "a refusal must never echo the expected credential"
        );
    }

    // a WRONG credential is a distinct, and distinctly named, refusal.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let mut guessed = with_peer(
        post("/v1/admin/shutdown", serde_json::json!({})),
        "127.0.0.1:40000",
    );
    guessed.headers_mut().insert(
        noded::admin::ADMIN_TOKEN_HEADER,
        "deadbeef".parse().unwrap(),
    );
    let response = noded::router(handle).oneshot(guessed).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["reason"],
        "operator_token_mismatch"
    );

    // and the operator still gets through — a gate that locks the owner out is
    // a worse bug than the one it closes.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let signal = handle.clone();
    let response = noded::router(handle)
        .oneshot(with_operator(with_peer(
            post("/v1/admin/shutdown", serde_json::json!({})),
            "127.0.0.1:40000",
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // wait on the system's own event, not on a duration.
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        signal.shutdown_requested(),
    )
    .await
    .expect("the operator's shutdown reached the node");
}

/// the stage lane's body cap is EXPLICIT, and over it is a NAMED refusal.
///
/// Two cliffs, one test. Without a `DefaultBodyLimit` layer axum applies its
/// implicit 2 MiB default, and `crates/modules/apps/runs/component.wasm` is
/// already 1.73 MB of that — so the next module to grow would have become
/// un-stageable behind an opaque tower error with no reason token. Above the
/// real cap the refusal must still be a reason a client can branch on.
#[tokio::test]
async fn the_module_stage_body_cap_is_explicit_and_its_refusal_is_named() {
    fn stage(body: Vec<u8>) -> Request<Body> {
        with_operator(with_peer(
            Request::builder()
                .method("POST")
                // fanout=false: this handle wires no code plane, and the
                // network fan-out is not what the body cap is about.
                .uri("/v1/admin/module-code/stage?fanout=false")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(body))
                .unwrap(),
            "127.0.0.1:40000",
        ))
    }

    // 3 MiB — over axum's implicit default, under ours. The cliff is gone.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        .oneshot(stage(
            module_artifact::Artifact::module(vec![7u8; 3 * 1024 * 1024]).encode(),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an artifact past axum's implicit 2 MiB default must still stage"
    );

    // over the explicit cap — refused, with a token rather than tower's prose.
    let (handle, cmd_rx) = operator_handle();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        .oneshot(stage(vec![7u8; 16 * 1024 * 1024 + 1]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body_json(response).await["reason"],
        "module_artifact_too_large"
    );
}

/// FAIL CLOSED: a node that minted no operator credential verifies nothing, so
/// it refuses every admin request. There is no "unauthenticated if unset" arm —
/// that fallback IS the hole this gate closes.
#[tokio::test]
async fn a_node_with_no_minted_credential_refuses_every_admin_request() {
    // NOT `local_node()`: this is the node whose mint FAILED, so it carries the
    // bare default config and has nothing to verify against.
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    spawn_fake_actor(cmd_rx, None);
    // the default config carries no token, and the default exposure is Loopback.
    assert_eq!(AdminConfig::default().operator_token, None);
    let response = noded::router(handle)
        .oneshot(with_operator(with_peer(
            post("/v1/admin/shutdown", serde_json::json!({})),
            "127.0.0.1:40000",
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(response).await["reason"],
        "operator_token_unavailable"
    );
}

/// an actor that answers exactly one thing: `identity` `OfKey` for the
/// operator's `owner_key` → the account it is on, whose sole member it is.
/// everything else is a module error (the admin owner path only ever asks this
/// one question).
fn spawn_owner_actor(mut cmds: mpsc::Receiver<NodeCommand>, owner_key: Vec<u8>) {
    tokio::spawn(async move {
        while let Some(cmd) = cmds.next().await {
            if let NodeCommand::Query { target, req, reply } = cmd {
                assert_eq!(target, "identity");
                let identity::IdentityQuery::OfKey { key } =
                    identity::decode_query(&req).expect("an identity query")
                else {
                    panic!("the admin owner path asks only OfKey");
                };
                assert_eq!(
                    key, owner_key,
                    "the owner path resolves the operator's own key"
                );
                let view = identity::AccountView {
                    number: 1,
                    name: "owner".into(),
                    control: identity::Control::Keys,
                    keys: vec![identity::KeyView {
                        scheme: identity::KeyScheme::Ed25519,
                        pubkey: owner_key.clone(),
                        label: None,
                        added_at: 0,
                    }],
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                };
                let bytes = identity::encode_reply(&identity::IdentityReply::Account(Some(view)));
                let _ = reply.send(Ok(bytes));
            }
        }
    });
}

fn admin_signed_post(
    uri: &str,
    signer: &commonware_cryptography::ed25519::PrivateKey,
    claimed_key_hex: &str,
    node_key: &[u8],
    ts: u64,
) -> Request<Body> {
    let sig = noded::admin::sign_admin(signer, "POST", uri, node_key, ts);
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(noded::admin::ADMIN_KEY_HEADER, claimed_key_hex)
        .header(noded::admin::ADMIN_TS_HEADER, ts.to_string())
        .header(
            noded::admin::ADMIN_SIG_HEADER,
            duckfs_core::to_hex(sig.as_ref()),
        )
        .body(Body::from("{}"))
        .unwrap()
}

/// the full `Public` owner path over the actor lane: unsigned is refused, a
/// non-owner signature is refused, the committed owner's signature passes.
#[tokio::test]
async fn public_admin_enforces_the_committed_owner_pop() {
    use commonware_cryptography::Signer as _;
    let owner = commonware_cryptography::ed25519::PrivateKey::from_seed(77);
    let owner_key = owner.public_key().as_ref().to_vec();
    let node_key = vec![0xabu8; 32];
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let owner_key_hex = duckfs_core::to_hex(&owner_key);

    // the handle carries a REAL operator credential on purpose: a `Public` node
    // with a committed owner must be on the owner path and nothing else, so
    // every assertion below is made against a node that HAS the other secret.
    let mk_handle = || {
        let (handle, cmd_rx, _e) = local_node();
        spawn_owner_actor(cmd_rx, owner_key.clone());
        handle.with_admin(AdminConfig {
            exposure: AdminExposure::Public,
            node_key: Some(node_key.clone()),
            owner_key: Some(owner_key.clone()),
            operator_token: Some(OPERATOR.to_string()),
            ..Default::default()
        })
    };

    // unsigned ⇒ 401.
    let bare = noded::router(mk_handle())
        .oneshot(post("/v1/admin/shutdown", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        bare.status(),
        StatusCode::UNAUTHORIZED,
        "public admin needs a signature"
    );

    // signed by a non-owner (valid PoP, wrong account) ⇒ 403.
    let attacker = commonware_cryptography::ed25519::PrivateKey::from_seed(99);
    let attacker_hex = duckfs_core::to_hex(attacker.public_key().as_ref());
    let forged = noded::router(mk_handle())
        .oneshot(admin_signed_post(
            "/v1/admin/shutdown",
            &attacker,
            &attacker_hex,
            &node_key,
            ts,
        ))
        .await
        .unwrap();
    assert_eq!(
        forged.status(),
        StatusCode::FORBIDDEN,
        "a non-owner signer is refused"
    );

    // the owner's signature bound to a DIFFERENT node ⇒ 401 (cross-node replay).
    let replayed = noded::router(mk_handle())
        .oneshot(admin_signed_post(
            "/v1/admin/shutdown",
            &owner,
            &owner_key_hex,
            &[0xcd; 32],
            ts,
        ))
        .await
        .unwrap();
    assert_eq!(
        replayed.status(),
        StatusCode::UNAUTHORIZED,
        "a signature minted for another node is refused here"
    );

    // THE cross-check: the operator credential is NOT an alternative credential
    // here. Once an owner is committed, `Public` is the owner path and only the
    // owner path — otherwise anyone who can read the workspace would keep a
    // standing bypass around the very PoP that `Public` exposure exists for,
    // and the two gates would be an OR instead of a ladder. A loopback peer
    // presenting a VALID operator token and no signature is still refused.
    let mut token_only = with_operator(with_peer(
        post("/v1/admin/shutdown", serde_json::json!({})),
        "127.0.0.1:40000",
    ));
    // and a smuggled owner-key header changes nothing without the signature.
    token_only.headers_mut().insert(
        noded::admin::ADMIN_KEY_HEADER,
        owner_key_hex.parse().unwrap(),
    );
    let refused = noded::router(mk_handle())
        .oneshot(token_only)
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        StatusCode::UNAUTHORIZED,
        "the operator token must not stand in for the owner PoP"
    );
    assert_eq!(
        body_json(refused).await["reason"],
        "owner_signature_invalid",
        "a token-only caller must fail the OWNER check, not pass some operator arm"
    );

    // the committed owner's signature for THIS node ⇒ 200.
    let ok = noded::router(mk_handle())
        .oneshot(admin_signed_post(
            "/v1/admin/shutdown",
            &owner,
            &owner_key_hex,
            &node_key,
            ts,
        ))
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "the node owner may drive control"
    );
}

/// the loopback gate FAILS CLOSED: a request with no ConnectInfo at all (an
/// embedder that forgot the connect-info make-service) is refused, never
/// granted local trust.
#[tokio::test]
async fn a_peer_without_connect_info_is_refused() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);
    let response = noded::router(handle)
        // `post()` stamps a loopback peer, which is exactly what this test must
        // NOT have: the request arrives with no ConnectInfo at all.
        .oneshot(with_operator(admin_request("POST", "/v1/admin/shutdown")))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "an unknown peer must not inherit loopback trust"
    );
}

#[tokio::test]
async fn a_dead_actor_maps_to_service_unavailable() {
    let (handle, cmd_rx, _events) = local_node();
    drop(cmd_rx); // no actor at all

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({ "target": "chat", "payload": {} }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

fn gateway_route() -> gateway::RouteRecord {
    gateway::RouteRecord {
        statement: gateway::RouteStatement {
            chain_id: "test".into(),
            account_id: 1,
            name: gateway::RouteName::named("app"),
            publisher_node: vec![2; 32],
            revision: 7,
            route: Some(gateway::RouteDefinition {
                target: gateway::RouteTarget::LoopbackHttp,
                policy: gateway::RoutePolicy {
                    audience: gateway::RouteAudience::Network,
                    methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                    max_request_bytes: Some(1024),
                    max_response_bytes: 4096,
                    allow_authorization: false,
                    allow_upgrade: false,
                },
            }),
        },
        authorization: gateway::MemberAuthorization {
            signer: vec![3; 32],
            signature: vec![4; 64],
        },
    }
}

/// Answer the duck:// browser proxy's queries against the MERGED gateway
/// module: a handle `Resolve` returns the route's account, a `Get` returns the
/// signed route — both now target the one "gateway" module, dispatched by
/// query variant. `queries` is the total count.
fn spawn_duck_actor(mut cmds: mpsc::Receiver<NodeCommand>, queries: usize) {
    tokio::spawn(async move {
        for _ in 0..queries {
            let NodeCommand::Query { target, req, reply } = cmds.next().await.unwrap() else {
                panic!("gateway only issues queries");
            };
            assert_eq!(target, "gateway");
            let bytes = match gateway::decode_query(&req).unwrap() {
                gateway::GatewayQuery::Resolve { .. } => {
                    gateway::encode_reply(&gateway::GatewayReply::Resolved(Some(
                        gateway::ResolvedAccount { account_id: 1 },
                    )))
                }
                gateway::GatewayQuery::Get { .. } => gateway::encode_reply(
                    &gateway::GatewayReply::Route(Box::new(Some(gateway_route()))),
                ),
                other => panic!("unexpected query {other:?}"),
            };
            let _ = reply.send(Ok(bytes));
        }
    });
}

fn spawn_gateway_actor(mut cmds: mpsc::Receiver<NodeCommand>, replies: usize) {
    tokio::spawn(async move {
        for _ in 0..replies {
            let NodeCommand::Query { target, req, reply } = cmds.next().await.unwrap() else {
                panic!("gateway only queries route state");
            };
            assert_eq!(target, "gateway");
            assert_eq!(
                gateway::decode_query(&req).unwrap(),
                gateway::GatewayQuery::Get {
                    account_id: 1,
                    name: gateway::RouteName::named("app"),
                }
            );
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(gateway_route())),
            ))));
        }
    });
}

#[tokio::test]
async fn gateway_proxy_resolves_the_signed_route_and_forwards_post_body() {
    use base64::Engine as _;
    let (handle, cmds, _events) = local_node();
    spawn_gateway_actor(cmds, 1);
    let (lane, mut jobs) = tokio::sync::mpsc::channel::<noded::GatewayJob>(1);
    tokio::spawn(async move {
        let job = jobs.recv().await.expect("one gateway job");
        let noded::GatewayJob::Http {
            publisher_node,
            head,
            body,
            reply,
            ..
        } = job
        else {
            panic!("expected an http gateway job");
        };
        assert_eq!(publisher_node, [2; 32]);
        assert_eq!(head.name, gateway::RouteName::named("app"));
        assert_eq!(head.method, gateway::RouteMethod::Post);
        assert_eq!(head.path_and_query, "/api/items");
        assert_eq!(request_body(body).await, br#"{"name":"duck"}"#);
        let _ = reply.send(Ok(noded::GatewayResponse {
            head: gateway::ProxyResponseHead {
                status: 201,
                headers: vec![gateway::ProxyHeader {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
            },
            body: {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                tx.try_send(Ok(bytes::Bytes::from_static(br#"{"ok":true}"#)))
                    .unwrap();
                drop(tx);
                rx
            },
        }));
    });
    let request_body = br#"{"name":"duck"}"#;
    let request = post(
        "/v1/gateway/proxy",
        serde_json::json!({
            "head": {
                "account_id": 1,
                "name": { "label": "app" },
                "revision": 7,
                "method": "post",
                "path_and_query": "/api/items",
                "headers": [{ "name": "content-type", "value": "application/json" }],
                "upgrade": false,
            },
            "body_b64": base64::engine::general_purpose::STANDARD.encode(request_body),
        }),
    );
    let response = noded::router(handle.with_gateway(lane))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["head"]["status"], 201);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(body["body_b64"].as_str().unwrap())
            .unwrap(),
        br#"{"ok":true}"#
    );
}

#[tokio::test]
async fn gateway_operator_forwards_existing_authentication_without_requiring_an_account() {
    let (handle, cmds, _events) = local_node();
    let owner = caller();
    let handle = handle.with_admin(AdminConfig {
        operator_token: Some(OPERATOR.into()),
        node_key: Some(NODE_KEY.to_vec()),
        owner_key: Some(owner.public_key().as_ref().to_vec()),
        ..Default::default()
    });
    let body = serde_json::json!({
        "head": {
            "account_id": 1, "name": {"label": "app"}, "revision": 7,
            "method": "post", "path_and_query": "/sessions", "headers": [],
            "upgrade": false, "user_pop": null,
        },
        "body_b64": "e30=",
    });
    let (lane, mut jobs) = tokio::sync::mpsc::channel(1);
    let app = noded::router(handle.with_gateway(lane));
    let path = "/v1/gateway/operator";
    let stranger = commonware_cryptography::ed25519::PrivateKey::from_seed(78);
    let requests = [
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        signed_by(&stranger, "POST", path, body.clone()),
        with_peer(post(path, body.clone()), "192.0.2.1:40000"),
    ];
    for (request, status) in requests.into_iter().zip([
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::UNAUTHORIZED,
    ]) {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
    }
    let mut forged = body.clone();
    forged["head"]["operator"] = true.into();
    let response = app
        .clone()
        .oneshot(post("/v1/gateway/proxy", forged))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(jobs.try_recv().is_err());

    spawn_gateway_actor(cmds, 2);
    let serving = tokio::spawn(async move {
        for _ in 0..2 {
            let noded::GatewayJob::Http {
                head, body, reply, ..
            } = jobs.recv().await.unwrap()
            else {
                panic!("HTTP exchange expected");
            };
            assert!(head.operator);
            assert!(head.user_pop.is_none());
            assert!(head.headers.is_empty());
            assert_eq!(request_body(body).await, b"{}");
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            drop(tx);
            reply
                .send(Ok(noded::GatewayResponse {
                    head: gateway::ProxyResponseHead {
                        status: 204,
                        headers: vec![],
                    },
                    body: rx,
                }))
                .unwrap();
        }
    });
    for request in [
        post(path, body.clone()),
        signed_by(&owner, "POST", path, body),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["head"]["status"], 204);
    }
    serving.await.unwrap();
}

#[tokio::test]
async fn gateway_api_rejects_untrusted_browser_origins_before_network_work() {
    let (handle, _cmds, _events) = local_node();
    for origin in ["https://evil.example", "http://app.demo.duck"] {
        let mut request = Request::builder()
            .method("GET")
            .uri("/v1/gateway/browser")
            .body(Body::empty())
            .unwrap();
        request
            .headers_mut()
            .insert(header::ORIGIN, origin.parse().unwrap());
        request
            .headers_mut()
            .insert("sec-fetch-site", "cross-site".parse().unwrap());
        let response = noded::router(handle.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "accepted {origin}"
        );
    }
}

#[tokio::test]
async fn gateway_browser_proxy_is_duck_origin_scoped_and_cross_origin_safe() {
    let (handle, cmds, _events) = local_node();
    // One proxy request resolves once via duckdns and twice via gateway
    // (the revision pre-check plus proxy_current's own resolution).
    spawn_duck_actor(cmds, 3);
    let (lane, mut jobs) = tokio::sync::mpsc::channel::<noded::GatewayJob>(1);
    let handle = handle
        .with_gateway(lane)
        .with_browser_gateway("127.0.0.1:49152".parse().unwrap());

    tokio::spawn(async move {
        let job = jobs.recv().await.unwrap();
        let noded::GatewayJob::Http {
            head, body, reply, ..
        } = job
        else {
            panic!("expected an http gateway job");
        };
        assert_eq!(head.method, gateway::RouteMethod::Post);
        assert_eq!(head.path_and_query, "/api");
        assert_eq!(request_body(body).await, b"payload");
        let _ = reply.send(Ok(noded::GatewayResponse {
            head: gateway::ProxyResponseHead {
                status: 201,
                headers: vec![gateway::ProxyHeader {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
            },
            body: {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                tx.try_send(Ok(bytes::Bytes::from_static(br#"{"ok":true}"#)))
                    .unwrap();
                drop(tx);
                rx
            },
        }));
    });
    let authority = "app.demo.duck";
    let origin = format!("duck://{authority}");
    let response = noded::gateway_browser_router(handle.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api")
                .header("x-duck-authority", authority)
                .header(header::ORIGIN, &origin)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("payload"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["access-control-allow-origin"], origin);
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains(&format!("connect-src {origin} ws://127.0.0.1:49152")));
    assert!(csp.contains("worker-src 'none'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("sandbox allow-scripts allow-same-origin allow-forms"));
    assert!(csp.contains("webrtc 'block'"));

    // A page whose Origin does not match the forwarded authority is rejected
    // before any network work.
    let response = noded::gateway_browser_router(handle.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api")
                .header("x-duck-authority", authority)
                .header(header::ORIGIN, "duck://evil.demo.duck")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // A request the scheme handler did not stamp with an authority is refused.
    let response = noded::gateway_browser_router(handle)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
}

/// The browser door never holds a request body, so it cannot weigh one before
/// handing it on: it counts the frames as they pass, under the RESOLVED
/// route's own `max_request_bytes` rather than any router-wide limit, and the
/// byte past that cap turns the body itself into a named refusal. The job
/// exists — a streamed request is a job the instant its head is known — but
/// nothing past the cap is ever carried in it.
#[tokio::test]
async fn gateway_browser_proxy_streams_the_body_under_the_routes_own_cap() {
    let (handle, cmds, _events) = local_node();
    spawn_duck_actor(cmds, 2);
    let (lane, mut jobs) = tokio::sync::mpsc::channel::<noded::GatewayJob>(1);
    let handle = handle
        .with_gateway(lane)
        .with_browser_gateway("127.0.0.1:49152".parse().unwrap());

    let authority = "app.demo.duck";
    let cap = gateway_route()
        .statement
        .route
        .unwrap()
        .policy
        .max_request_bytes
        .expect("the fixture route caps its body") as usize;
    let door = tokio::spawn(async move {
        noded::gateway_browser_router(handle)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api")
                    .header("x-duck-authority", authority)
                    .header(header::ORIGIN, format!("duck://{authority}"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(vec![7u8; cap + 1]))
                    .unwrap(),
            )
            .await
            .unwrap()
    });

    let noded::GatewayJob::Http {
        mut body, reply, ..
    } = jobs.recv().await.expect("the head becomes a job")
    else {
        panic!("a POST is an Http job");
    };
    let mut carried = 0usize;
    let refusal = loop {
        match body.recv().await.expect("the body ends in a refusal") {
            Ok(chunk) => carried += chunk.len(),
            Err(failure) => break failure,
        }
    };
    assert!(
        matches!(refusal, noded::GatewayFailure::TooLarge(ref why)
            if why.starts_with("gateway_body_exceeds_route_cap") && why.contains(&cap.to_string())),
        "{refusal:?}"
    );
    assert!(
        carried <= cap,
        "{carried} bytes crossed a {cap}-byte cap before the refusal"
    );
    assert!(
        body.recv().await.is_none(),
        "the body kept sending after its refusal"
    );

    // The publisher side is what turns that refusal into a status; answer as
    // it would so the door can finish.
    let _ = reply.send(Err(refusal));
    let response = door.await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let error = body_json(response).await["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.starts_with("gateway_body_exceeds_route_cap"),
        "{error}"
    );
    assert!(error.contains(&cap.to_string()), "{error}");
}

/// a GET carrying the RFC 6455 upgrade headers axum's `WebSocketUpgrade`
/// extractor checks. NOTE the oneshot transport can never actually upgrade:
/// hyper's `OnUpgrade` state only exists on a real served connection, so the
/// extractor stops these requests with 426 BEFORE the handler body runs. that
/// still separates "route is wired" (426) from "route is gone" (404); the
/// handler's own no-hub refusal (503 + a body that says why) is exercised
/// against the real spawned binary in `daemon_e2e.rs`.
fn ws_upgrade(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn call_product_route_is_absent() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(ws_upgrade("/v1/call/ws?channel=general"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_presence_ws_route_is_wired() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(ws_upgrade("/v1/presence/ws?page=page-1"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_old_voice_ws_route_is_gone() {
    // app and node ship lockstep: `/v1/voice/ws` was replaced by `/v1/call/ws`,
    // so the old path is simply unrouted now — a 404, not a refusal.
    let (handle, cmd_rx, _events) = local_node();
    spawn_fake_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(ws_upgrade("/v1/voice/ws?channel=general"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---- duckfs read/probe surface: refs / diff / has-chunks --------------------

use std::collections::BTreeMap;

use duckfs_core::{DiffEntry, DiffKind, FilesQuery, FilesReply, RefsInfo};

/// a scripted files actor: decodes each `FilesQuery` and answers the matching
/// canned `FilesReply`, or fails a submit with `submit_err` (the 400-envelope
/// contract test). proves the router forwards `target = "files"` and translates
/// the typed reply to json without a live module.
fn spawn_files_actor(
    mut cmds: futures::channel::mpsc::Receiver<NodeCommand>,
    submit_err: Option<&'static str>,
) {
    tokio::spawn(async move {
        while let Some(cmd) = cmds.next().await {
            match cmd {
                NodeCommand::Query { target, req, reply } => {
                    assert_eq!(target, "files");
                    let q = duckfs_core::decode_query(&req).expect("files query decodes");
                    let bytes = match q {
                        FilesQuery::Refs {} => {
                            duckfs_core::encode_reply(&FilesReply::Refs(RefsInfo {
                                head: Some("ab".repeat(32)),
                                pins: BTreeMap::new(),
                                window_len: 4,
                            }))
                        }
                        FilesQuery::Diff { .. } => {
                            duckfs_core::encode_reply(&FilesReply::Diff(vec![DiffEntry {
                                path: "/a".into(),
                                kind: DiffKind::Modified,
                            }]))
                        }
                        FilesQuery::HasChunks { ids } => {
                            // present iff the id starts with "aa" — proves the reply
                            // order maps back to the request order over the wire.
                            let present = ids.iter().map(|id| id.starts_with("aa")).collect();
                            duckfs_core::encode_reply(&FilesReply::HasChunks { present })
                        }
                        other => panic!("unexpected files query: {other:?}"),
                    };
                    let _ = reply.send(Ok(bytes));
                }
                NodeCommand::Submit { target, reply, .. } => {
                    assert_eq!(target, "files");
                    let result = match submit_err {
                        Some(err) => Err(err.to_string()),
                        None => Ok(BlockSummary {
                            height: 9,
                            root_hash: "ab".repeat(32),
                        }),
                    };
                    let _ = reply.send(result);
                }
                _ => {}
            }
        }
    });
}

#[tokio::test]
async fn files_refs_route_returns_head() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_files_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/query",
            serde_json::json!({"target":"files","query":{"refs":{}}}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["refs"]["head"], "ab".repeat(32));
    assert_eq!(body["refs"]["window_len"], 4);
}

#[tokio::test]
async fn files_diff_route_returns_entries() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_files_actor(cmd_rx, None);

    let response = noded::router(handle)
        .oneshot(post("/v1/query", serde_json::json!({"target":"files","query":{"diff":{"from":"aa","to":"bb","prefix":"/"}}})))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["diff"][0]["path"], "/a");
    assert_eq!(body["diff"][0]["kind"], "modified");
}

#[tokio::test]
async fn files_has_chunks_route_preserves_request_order() {
    let (handle, cmd_rx, _events) = local_node();
    spawn_files_actor(cmd_rx, None);

    let present = "aa".repeat(32);
    let absent = "bb".repeat(32);
    let response = noded::router(handle)
        .oneshot(post(
            "/v1/query",
            serde_json::json!({"target":"files","query":{"has_chunks":{"ids":[present,absent]}}}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["has_chunks"]["present"],
        serde_json::json!([true, false])
    );
}

#[tokio::test]
async fn files_module_rejection_is_a_verbatim_400_envelope() {
    // the engine's conflict taxonomy keys on the module error string arriving
    // untouched inside a 400 {"error": "files: ..."} — pin the envelope here.
    let (handle, cmd_rx, _events) = local_node();
    spawn_files_actor(cmd_rx, Some("files: conflict: /x changed since base"));

    let response = noded::router(handle)
        .oneshot(post(
            "/v1/submit",
            serde_json::json!({ "target":"files", "payload":{"commit":{"base_snapshot":null,"message":"m","changes":[]}} }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"], "files: conflict: /x changed since base");
}

// ---- duckfs workspace RPC: 503 when unconfigured, slug validation -----------

#[tokio::test]
async fn fs_workspaces_is_503_when_unconfigured() {
    // a handle that never injected the workspace root (the fake actor's) answers
    // the seam with a clean 503, not a panic. no actor needed: the config guard
    // returns before any command crosses the lane.
    let (handle, _cmd_rx, _events) = local_node();

    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/fs/workspaces",
            serde_json::json!({ "prefix": "/shared/x" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn fs_workspace_commit_rejects_a_bad_slug() {
    // a non-`[a-z0-9]` id (here uppercase) is refused BEFORE any disk touch —
    // the slug guard is the traversal defense on the path param.
    let root = tempfile::tempdir().expect("workspace root");
    let (handle, _cmd_rx, _events) = local_node();
    let handle = handle.with_duckfs_workspaces(root.path().to_path_buf());

    let response = noded::router(handle)
        .oneshot(signed(
            "POST",
            "/v1/fs/workspaces/BAD/commit",
            serde_json::json!({ "message": "m" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"], "invalid workspace id");
}

/// The invite route, end to end through the real router.
///
/// Three properties, and each one is a way an operator gets hurt without it: a
/// daemon with no workspace must SAY it cannot mint rather than 500 or hang; a
/// nonsense TTL must be refused before the mint touches the descriptor (the
/// mint SAVES that file, so a refusal afterwards is a write nobody asked for);
/// and a successful mint must answer the blob itself, because the caller pastes
/// what comes back.
#[tokio::test]
async fn the_invite_route_mints_refuses_and_says_when_it_cannot() {
    let body_of = |response: axum::response::Response| async move {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("a bounded body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    };
    // minting is a mutation, so the caller presents a credential: here the
    // operator one, which is what the desktop app (the only in-tree minter)
    // reads out of the node's workspace.
    let post = |app: axum::Router, body: &'static str| async move {
        app.oneshot(with_operator(with_peer(
            Request::builder()
                .method("POST")
                .uri("/v1/invite")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("a request"),
            "127.0.0.1:40000",
        )))
        .await
        .expect("a response")
    };

    // no minter wired: the honest answer is "this daemon does not do that".
    let (handle, _cmds, _events) = local_node();
    let unwired = post(noded::router(handle), r#"{"ttl_days":7}"#).await;
    assert_eq!(unwired.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_of(unwired).await.contains("no invite minter"));

    // wired: the TTL reaches the minter, and the blob comes back whole.
    let (handle, _cmds, _events) = local_node();
    handle
        .status_cell()
        .wire_invite_minter(|ttl_days| Ok(format!("duck-invite-for-{ttl_days}-days")));
    let app = noded::router(handle);

    let minted = post(app.clone(), r#"{"ttl_days":7}"#).await;
    assert_eq!(minted.status(), StatusCode::OK);
    assert!(body_of(minted).await.contains("duck-invite-for-7-days"));

    // a TTL outside the bounds never reaches the minter — which means the
    // descriptor is never rewritten for a request that was going to be refused.
    for refused in [r#"{"ttl_days":0}"#, r#"{"ttl_days":4000}"#] {
        let response = post(app.clone(), refused).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{refused}");
        assert!(body_of(response).await.contains("ttl_days"));
    }

    // no body at all is the ONE default the CLI verb mints too, not an error:
    // `curl -XPOST /v1/invite` and `ducktape node invite` hand out the same invite.
    let defaulted = post(app, "{}").await;
    assert_eq!(defaulted.status(), StatusCode::OK);
    let expected = format!("for-{}-days", workspace_config::DEFAULT_INVITE_TTL_DAYS);
    assert!(body_of(defaulted).await.contains(&expected));
}

// ---- huddle: a remote device joins with its ACCOUNT key ---------------------
//
// a device pointed at a node it does not host holds no operator token and no
// workspace secret. what it does hold is the person's key, and that key is in
// the network's account directory. so the two huddle gates a device crosses
// (`/v1/huddle/node-proof`, then the `/v1/call/ws` upgrade) admit a signed
// request by a key that holds an account — the node-proof to any such key, the
// media socket only to one the channel's committed huddle roster names AT THIS
// node.

/// the mesh identity the huddle node answers as.
fn huddle_node_signer() -> commonware_cryptography::ed25519::PrivateKey {
    commonware_cryptography::ed25519::PrivateKey::from_seed(4242)
}

/// a member key with account 5; `caller()` holds no account.
fn member() -> commonware_cryptography::ed25519::PrivateKey {
    commonware_cryptography::ed25519::PrivateKey::from_seed(500)
}

const MEMBER_ACCOUNT: u64 = 5;

/// an actor holding one channel, `general`, whose huddle roster names
/// `member()` (as account 5) routed through `huddle_node_signer()`, and an
/// identity directory that knows `member()` and nobody else.
fn spawn_huddle_actor(mut cmds: mpsc::Receiver<NodeCommand>) {
    tokio::spawn(async move {
        while let Some(cmd) = cmds.next().await {
            let NodeCommand::Query { target, req, reply } = cmd else {
                panic!("the huddle gates only read");
            };
            let bytes = match target.as_str() {
                "identity" => {
                    let identity::IdentityQuery::OfKey { key } =
                        identity::decode_query(&req).unwrap()
                    else {
                        panic!("the huddle gates resolve a key to its account");
                    };
                    let is_member = key == member().public_key().as_ref().to_vec();
                    let account = is_member.then(|| identity::AccountView {
                        number: MEMBER_ACCOUNT,
                        name: "member".into(),
                        control: identity::Control::Keys,
                        keys: vec![identity::KeyView {
                            scheme: identity::KeyScheme::Ed25519,
                            pubkey: key,
                            label: None,
                            added_at: 1,
                        }],
                        avatar: None,
                        bio: None,
                        updated_at: 1,
                    });
                    identity::encode_reply(&identity::IdentityReply::Account(account))
                }
                "chat" => {
                    let chat::ChatQuery::Channel { channel_id } = chat::decode_query(&req).unwrap()
                    else {
                        panic!("the media gate reads one channel record");
                    };
                    let channel = (channel_id == "general").then(|| chat::Channel {
                        id: "general".into(),
                        name: "general".into(),
                        created_at: 1,
                        head_seq: 0,
                        post_policy: chat::PostPolicy::Open,
                        hooks: vec![],
                        pinned: vec![],
                        voice: false,
                        huddle: vec![chat::HuddleMember {
                            party: chat::Party::Account(MEMBER_ACCOUNT),
                            node: huddle_node_signer().public_key().as_ref().to_vec(),
                            joined_at: 1,
                        }],
                        owner: chat::Party::System,
                        archived: false,
                        revision: 1,
                    });
                    chat::encode_reply(&chat::ChatReply::Channel(channel))
                }
                other => panic!("unexpected query target {other}"),
            };
            let _ = reply.send(Ok(bytes));
        }
    });
}

fn huddle_node() -> NodeHandle {
    let (handle, cmd_rx, _events) = local_node();
    spawn_huddle_actor(cmd_rx);
    handle.with_node_signer(huddle_node_signer())
}

/// a ws upgrade signed the way the desktop app signs one for a remote node:
/// the data-plane trio over `GET`, the exact path+query, and an empty body.
fn signed_ws_upgrade(
    signer: &commonware_cryptography::ed25519::PrivateKey,
    uri: &str,
) -> Request<Body> {
    signed_ws_upgrade_over(signer, uri, uri)
}

/// [`signed_ws_upgrade`] with the signature minted over `signed_uri` while the
/// request goes to `uri` — the two differ only in the test that proves the
/// path binding.
fn signed_ws_upgrade_over(
    signer: &commonware_cryptography::ed25519::PrivateKey,
    uri: &str,
    signed_uri: &str,
) -> Request<Body> {
    let mut req = ws_upgrade(uri);
    for (name, value) in
        noded::signed_req::request_headers(signer, "GET", signed_uri, &NODE_KEY, b"")
    {
        req.headers_mut().insert(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_str(&value).unwrap(),
        );
    }
    req
}

#[tokio::test]
async fn huddle_node_proof_binds_the_signing_key_of_an_account_holder() {
    let response = noded::router(huddle_node())
        .oneshot(signed_by(
            &member(),
            "POST",
            "/v1/huddle/node-proof",
            serde_json::json!({ "channel_id": "general" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let minted = body_json(response).await;

    // the proof names THIS node and binds the SIGNER — never a caller-supplied
    // user — so a roster entry can only ever carry a node that agreed to
    // route that exact person.
    let node = huddle_node_signer().public_key();
    assert_eq!(minted["node"], noded::hex_bytes(node.as_ref()));
    let proof_hex = minted["node_proof"].as_str().unwrap();
    let proof: Vec<u8> = (0..proof_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&proof_hex[i..i + 2], 16).unwrap())
        .collect();
    let proof = commonware_cryptography::ed25519::Signature::try_from(proof.as_slice()).unwrap();
    let preimage = chat::huddle_join_preimage("general", member().public_key().as_ref());
    use commonware_cryptography::Verifier as _;
    assert!(node.verify(chat::HUDDLE_JOIN_NS, &preimage, &proof));
}

#[tokio::test]
async fn huddle_node_proof_refuses_a_key_that_holds_no_account() {
    let response = noded::router(huddle_node())
        .oneshot(signed_by(
            &caller(),
            "POST",
            "/v1/huddle/node-proof",
            serde_json::json!({ "channel_id": "general" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["reason"], "key_without_account");
}

#[tokio::test]
async fn huddle_node_proof_refuses_the_operator_credential() {
    // the proof binds a PERSON's key. the node acting as itself has no person
    // to bind, so the operator credential — which the guard admits on every
    // lane — is not a way to mint one.
    let response = noded::router(huddle_node())
        .oneshot(post(
            "/v1/huddle/node-proof",
            serde_json::json!({ "channel_id": "general" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn presence_ws_admits_a_signed_account_holder_and_refuses_a_keyless_one() {
    let response = noded::router(huddle_node())
        .oneshot(signed_ws_upgrade(&member(), "/v1/presence/ws?page=page-1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let response = noded::router(huddle_node())
        .oneshot(signed_ws_upgrade(&caller(), "/v1/presence/ws?page=page-1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["reason"], "key_without_account");
}

#[tokio::test]
async fn blob_upload_capacity_is_reserved_before_signature_body_collection() {
    let (handle, _commands, _events) = local_node();
    let router = noded::router(handle);
    let mut releases = Vec::new();
    let mut requests = Vec::new();
    for _ in 0..2 {
        let (started, polled) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let body = Body::from_stream(futures::stream::once(async move {
            started.send(()).unwrap();
            released.await.unwrap();
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"blob"))
        }));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/files/blob")
            .body(body)
            .unwrap();
        requests.push(tokio::spawn(router.clone().oneshot(request)));
        polled.await.unwrap();
        releases.push(release);
    }
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/v1/files/blob")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    for release in releases {
        release.send(()).unwrap();
    }
    for request in requests {
        assert_ne!(
            request.await.unwrap().unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    assert_ne!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn operator_stream_binds_the_target_in_the_signature_and_forwards_without_an_account() {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};
    let (handle, mut cmds, _events) = local_node();
    let owner = caller();
    let handle = handle.with_admin(AdminConfig {
        operator_token: Some(OPERATOR.into()),
        node_key: Some(NODE_KEY.to_vec()),
        owner_key: Some(owner.public_key().as_ref().to_vec()),
        ..Default::default()
    });
    let (lane, mut jobs) = tokio::sync::mpsc::channel(1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            noded::router(handle.with_gateway(lane))
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let head = serde_json::json!({
        "account_id": 1, "name": {"label":"app"}, "revision": 7,
        "method": "get", "path_and_query": "/sessions/0000000000000001?after=3",
        "headers": [], "upgrade": true, "user_pop": null,
    });
    let mut url = reqwest::Url::parse(&format!("ws://{address}/v1/gateway/operator")).unwrap();
    url.query_pairs_mut().append_pair("head", &head.to_string());
    let uri = format!("{}?{}", url.path(), url.query().unwrap());
    let mut signed_request = url.as_str().into_client_request().unwrap();
    for (name, value) in noded::signed_req::request_headers(&owner, "GET", &uri, &NODE_KEY, &[]) {
        signed_request
            .headers_mut()
            .insert(name, value.parse().unwrap());
    }
    let mut tampered = signed_request.clone();
    *tampered.uri_mut() = format!("{url}&changed=1").parse().unwrap();
    let tokio_tungstenite::tungstenite::Error::Http(refused) =
        tokio_tungstenite::connect_async(tampered)
            .await
            .unwrap_err()
    else {
        panic!("HTTP refusal")
    };
    assert_eq!(refused.status(), 401);
    assert!(jobs.try_recv().is_err());
    let actor = tokio::spawn(async move {
        for _ in 0..2 {
            let NodeCommand::Query { target, reply, .. } = cmds.next().await.unwrap() else {
                panic!("route query")
            };
            assert_eq!(target, "gateway");
            let mut route = gateway_route();
            route.statement.route.as_mut().unwrap().policy.allow_upgrade = true;
            reply
                .send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                    Box::new(Some(route)),
                ))))
                .unwrap();
            let noded::GatewayJob::Upgrade {
                head,
                to_browser,
                mut from_browser,
                ..
            } = jobs.recv().await.unwrap()
            else {
                panic!("upgrade")
            };
            assert!(head.operator);
            assert!(head.user_pop.is_none());
            assert_eq!(head.path_and_query, "/sessions/0000000000000001?after=3");
            to_browser
                .send(noded::GatewayWsMsg::Text("ready".into()))
                .await
                .unwrap();
            assert_eq!(
                from_browser.recv().await,
                Some(noded::GatewayWsMsg::Text("input".into()))
            );
        }
    });
    let mut token_request = url.as_str().into_client_request().unwrap();
    token_request
        .headers_mut()
        .insert("x-ducktape-admin-token", OPERATOR.parse().unwrap());
    for request in [signed_request, token_request] {
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap().to_text().unwrap(),
            "ready"
        );
        socket.send(Message::Text("input".into())).await.unwrap();
    }
    actor.await.unwrap();
    server.abort();
    let _ = server.await;
}
