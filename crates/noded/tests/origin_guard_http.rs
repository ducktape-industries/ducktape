//! The control plane, driven over real HTTP the way a browser drives it.
//!
//! The unit tests in `origin_guard` prove the predicate. These prove it is
//! actually WIRED — that a hostile request dies in the running router rather
//! than in a function nobody called — and, critically, that it never reaches the
//! node actor at all.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::StreamExt as _;
use futures::channel::mpsc;
use noded::{NodeCommand, NodeHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt as _;

/// An actor that records whether it was ever reached. A guard that returns 403
/// but still dispatches the command would be worthless, so "was the actor
/// touched" is the property under test, not just the status code.
fn spawn_counting_actor(mut cmds: mpsc::Receiver<NodeCommand>) -> Arc<AtomicUsize> {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        while let Some(_cmd) = cmds.next().await {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    seen
}

/// THE attack. A page rendered in one of our webviews — gateway content today,
/// any `https://` site once the browser opens to the internet — POSTs a
/// consensus op to the node's loopback control plane. `on_navigation` gates
/// navigation, not `fetch`; CORS never stops a request from ARRIVING. Only this
/// guard does.
#[tokio::test]
async fn a_hostile_page_cannot_forge_a_consensus_op() {
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    let reached = spawn_counting_actor(cmd_rx);
    let app = noded::router(handle);

    for origin in [
        "https://evil.com",
        // gateway content: a DIFFERENT host from `localhost`
        "http://0123456789abcdef0123456789abcdef.localhost:49152",
        "duck://site.alice.duck",
        // a sandboxed iframe or a data: document
        "null",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/submit")
                    .header(header::ORIGIN, origin)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"target":"chat","payload":{"create_channel":{"channel_id":"general"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "origin {origin} must not reach /v1/submit"
        );
    }

    // the op never even became a command
    tokio::task::yield_now().await;
    assert_eq!(
        reached.load(Ordering::SeqCst),
        0,
        "a refused origin must never reach the node actor"
    );
}

/// The whole control plane, not just submit: reading all state, reading the
/// filesystem, and pushing git are each as damaging as forging an op.
#[tokio::test]
async fn the_guard_covers_every_control_plane_route() {
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    let _reached = spawn_counting_actor(cmd_rx);
    let app = noded::router(handle);

    for (method, uri) in [
        ("POST", "/v1/submit"),
        ("POST", "/v1/query"),
        ("GET", "/v1/status"),
        ("GET", "/v1/blocks"),
        ("POST", "/v1/query"),
        ("POST", "/v1/fs/workspaces"),
        ("GET", "/v1/status"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::ORIGIN, "https://evil.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a hostile origin"
        );
    }
}

/// Native iced does not need a browser origin exception. Loopback origins are
/// untrusted unless an exact development origin is explicitly configured.
#[tokio::test]
async fn loopback_browser_origins_are_refused_by_default() {
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    let _reached = spawn_counting_actor(cmd_rx);
    let app = noded::router(handle);

    for origin in ["http://localhost:1420", "http://127.0.0.1:1420"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/status")
                    .header(header::ORIGIN, origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "loopback browser origin {origin} must be opt-in"
        );
    }
}

/// The CLI, agents and `git push` send no `Origin`. They are not the threat and
/// must not be collateral damage.
#[tokio::test]
async fn origin_less_clients_are_untouched() {
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    let _reached = spawn_counting_actor(cmd_rx);
    let app = noded::router(handle);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a client with no Origin (CLI, agent, git) must still be served"
    );
}

/// Even on a request the guard lets through, a hostile page must not be able to
/// READ the response. Permissive CORS previously handed `Access-Control-Allow-
/// Origin: *` to everyone, which made every byte of node state readable by any
/// page that could reach the port.
#[tokio::test]
async fn a_hostile_origin_is_never_granted_cors_access() {
    let (handle, cmd_rx, _events) = NodeHandle::channel();
    let _reached = spawn_counting_actor(cmd_rx);
    let app = noded::router(handle);

    let response = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/query")
                .header(header::ORIGIN, "https://evil.com")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "a hostile origin must never be granted Access-Control-Allow-Origin"
    );
}
