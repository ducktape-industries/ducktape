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

/// THE PUSH DOOR HAS NO CEILING. This body is larger than the whole transfer
/// limit that used to bound it (95.25 MiB — `127 * 768 KiB`, the relay's old
/// mailbox-sized cap), and the door does not answer 413: it spools the body to
/// disk and the push reaches the same proof gate every other push reaches.
///
/// A ceiling here is a ceiling on what anyone may push, and a repository's
/// first push carries its whole history. The refusal below is about the PROOF,
/// which is the only thing a push is ever refused for.
#[tokio::test]
async fn a_push_far_past_the_old_transfer_limit_is_not_refused_for_its_size() {
    const OLD_TRANSFER_LIMIT: usize = 127 * 768 * 1024;
    let (_directory, router) = application();
    let mut body = unsigned_push_body().into_bytes();
    body.extend(std::iter::repeat_n(0x42, OLD_TRANSFER_LIMIT + 1024 * 1024));
    let request = Request::builder()
        .method("POST")
        .uri("/lab/git-receive-pack")
        .header(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-request",
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.oneshot(authenticated(request)).await.unwrap();
    assert_ne!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a push is never refused for its size"
    );
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let report = String::from_utf8_lossy(&bytes);
    assert!(
        report.contains("git push --signed"),
        "the oversized push reached the proof gate: {report}"
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

/// a repo materialized on disk with one commit and every branch in `branches`
/// pointing at it. Returns that commit's oid as hex.
///
/// Every branch at the SAME oid is the ordinary shape right after a run cuts
/// its work branch, and it is exactly when an oid-only `HEAD` line stops being
/// enough: a client matching HEAD's oid against the advertised refs has more
/// than one answer to pick from.
fn seed_repo(store: &std::path::Path, name: &str, branches: &[&str]) -> String {
    let dir = store.join(name);
    let repo = git2::Repository::init(&dir).unwrap();
    let tree = repo.find_tree(repo.index().unwrap().write_tree().unwrap()).unwrap();
    let who = git2::Signature::now("Forge Test", "test@ducktape.local").unwrap();
    let oid = repo
        .commit(
            Some(&format!("refs/heads/{}", branches[0])),
            &who,
            &who,
            "seed",
            &tree,
            &[],
        )
        .unwrap();
    let commit = repo.find_commit(oid).unwrap();
    for branch in &branches[1..] {
        repo.branch(branch, &commit, true).unwrap();
    }
    oid.to_string()
}

/// the upload-pack ref advertisement for `repo`, as text.
async fn advertisement(router: axum::Router, repo: &str) -> String {
    let response = router
        .oneshot(authenticated(
            Request::builder()
                .uri(format!("/{repo}/info/refs?service=git-upload-pack"))
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// THE DEFAULT BRANCH IS THE MODULE'S, NOT THE LITERAL `main`. A repo seeded on
/// `dev` has no `main` at all: the advertisement used to emit a `HEAD` line
/// only for a branch by that name, so a clone was told nothing, fell back to
/// `refs/heads/main`, and landed on an unborn HEAD with every ref fetched.
#[tokio::test]
async fn a_dev_seeded_repo_advertises_its_integration_branch_as_head() {
    let (directory, router) = application();
    let head = seed_repo(directory.path(), "lab", &["dev", "agent/item-1"]);
    let body = advertisement(router, "lab").await;
    assert!(body.contains("symref=HEAD:refs/heads/dev"), "{body}");
    assert!(body.contains(&format!("{head} HEAD")), "{body}");
}

/// and `main` is still the answer when it is the only one of the two born —
/// the same order the forge module resolves a repo's head in.
#[tokio::test]
async fn a_main_only_repo_still_advertises_main_as_head() {
    let (directory, router) = application();
    let head = seed_repo(directory.path(), "lab", &["main", "agent/item-1"]);
    let body = advertisement(router, "lab").await;
    assert!(body.contains("symref=HEAD:refs/heads/main"), "{body}");
    assert!(body.contains(&format!("{head} HEAD")), "{body}");
}

/// a repo with neither born advertises no HEAD and no symref: there is no
/// default branch to name, and naming a feature branch would hand a clone a
/// checkout nobody asked for.
#[tokio::test]
async fn a_repo_with_no_default_branch_advertises_no_head() {
    let (directory, router) = application();
    seed_repo(directory.path(), "lab", &["agent/item-1"]);
    let body = advertisement(router, "lab").await;
    assert!(!body.contains("symref=HEAD"), "{body}");
    assert!(!body.contains(" HEAD\n"), "{body}");
}
