use super::*;
use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

fn application() -> (tempfile::TempDir, axum::Router) {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        node_url: "http://127.0.0.1:1".into(),
        node_key: "01".repeat(32),
        chain_id: "test-chain".into(),
        account: 1,
        label: "git".into(),
        module: "forge".into(),
        git_store: directory.path().into(),
        signing_seed: "09".repeat(32),
    };
    let router = router(config, [b'a'; 64]).unwrap();
    (directory, router)
}
fn authenticated(mut request: Request<Body>) -> Request<Body> {
    for (name, value) in [
        (
            "x-duck-upstream-token",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        ("x-duck-route-account", "1"),
        ("x-duck-route-label", "git"),
        ("x-duck-route-revision", "1"),
    ] {
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    request
}
#[tokio::test]
async fn caller_headers_without_handoff_secret_are_denied() {
    let (_directory, router) = application();
    let mut request = authenticated(
        Request::builder()
            .uri("/lab/info/refs?service=git-upload-pack")
            .body(Body::empty())
            .unwrap(),
    );
    request.headers_mut().remove("x-duck-upstream-token");
    request
        .headers_mut()
        .insert("x-duck-caller-account", "1".parse().unwrap());
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

/// one git pkt-line: a 4-hex length prefix over the payload INCLUDING itself.
fn pkt(payload: &str) -> String {
    format!("{:04x}{payload}", payload.len() + 4)
}

/// a stock (UNSIGNED) receive-pack body: one ref-update command, the flush that
/// ends the command list, and an empty pack.
fn unsigned_push_body() -> String {
    let zero = "0".repeat(40);
    let one = "1".repeat(40);
    format!(
        "{}0000",
        pkt(&format!("{zero} {one} refs/heads/main\0report-status\n"))
    )
}

/// A PUSH MUST PROVE ITSELF. An unsigned push used to be accepted and re-signed
/// with the node's key, so the first one to a new repo made this node's raw
/// pubkey the permanent owner (#1292).
#[tokio::test]
async fn an_unproven_push_is_refused() {
    let (_directory, router) = application();
    let request = Request::builder()
        .method("POST")
        .uri("/lab/git-receive-pack")
        .header(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-request",
        )
        .body(Body::from(unsigned_push_body()))
        .unwrap();
    let response = router.oneshot(authenticated(request)).await.unwrap();
    // CONSUME-AND-REFUSE: the pack was received, so the answer is git's own
    // report-status with the ref rejected — what git prints as
    // `! [remote rejected] main -> main (<reason>)`. An HTTP error here is
    // what git reports as "the remote end hung up unexpectedly", with the
    // reason lost.
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let report = String::from_utf8_lossy(&bytes);
    assert!(
        report.contains("unpack ok"),
        "a report-status answers the push: {report}"
    );
    assert!(
        report.contains("ng refs/heads/main "),
        "the ref is rejected, not the connection: {report}"
    );
    assert!(
        report.contains("git push --signed"),
        "the refusal names the two proofs a push can carry: {report}"
    );
}

/// An operator token cannot replace a user certificate at the service boundary.
#[tokio::test]
async fn an_operator_token_does_not_authorize_unsigned_pushes() {
    let (_directory, router) = application();
    let request = Request::builder()
        .method("POST")
        .uri("/lab/git-receive-pack")
        .header("x-ducktape-admin-token", "irrelevant-operator-secret")
        .body(Body::from(unsigned_push_body()))
        .unwrap();
    let response = router.oneshot(authenticated(request)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("git push --signed"));
}

#[tokio::test]
async fn upload_pack_have_round_returns_only_plain_nak() {
    fn pkt(payload: &[u8]) -> Vec<u8> {
        let mut line = format!("{:04x}", payload.len() + 4).into_bytes();
        line.extend_from_slice(payload);
        line
    }

    let oid = "11".repeat(20);
    let mut request_body = pkt(format!("want {oid} multi_ack_detailed side-band-64k\n").as_bytes());
    request_body.extend_from_slice(b"0000");
    request_body.extend_from_slice(&pkt(format!("have {oid}\n").as_bytes()));
    request_body.extend_from_slice(b"0000");

    let (_directory, router) = application();
    let response = router
        .oneshot(authenticated(
            Request::builder()
                .method("POST")
                .uri("/repo/git-upload-pack")
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-upload-pack-request",
                )
                .body(Body::from(request_body))
                .unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/x-git-upload-pack-result"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"0008NAK\n");
    assert!(!body.windows(4).any(|window| window == b"PACK"));
}
