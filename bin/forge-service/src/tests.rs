use super::*;
use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

fn application() -> (tempfile::TempDir, axum::Router) {
    application_on("test-chain", "http://127.0.0.1:1")
}

/// the service for `chain_id`, fronting the node at `node_url`.
fn application_on(chain_id: &str, node_url: &str) -> (tempfile::TempDir, axum::Router) {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        node_url: node_url.into(),
        node_key: "01".repeat(32),
        chain_id: chain_id.into(),
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
    unsigned_push_body_with_caps("report-status")
}

fn unsigned_push_body_with_caps(caps: &str) -> String {
    let zero = "0".repeat(40);
    let one = "1".repeat(40);
    format!(
        "{}0000",
        pkt(&format!("{zero} {one} refs/heads/main\0{caps}\n"))
    )
}

/// a SIGNED receive-pack body over the `chain-a`/`lab` fixture certificate,
/// framed as `git push --signed` frames it, followed by `pack` as the packfile.
fn signed_push_body(caps: &str, pack: &[u8]) -> Vec<u8> {
    use crate::git_http::receive_pack_tests::{ARMORED, CERT};
    let mut body = pkt(&format!("push-cert\0{caps}\n")).into_bytes();
    for line in CERT.split_inclusive('\n').chain(ARMORED.split_inclusive('\n')) {
        body.extend_from_slice(pkt(line).as_bytes());
    }
    body.extend_from_slice(pkt("push-cert-end\n").as_bytes());
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

/// the pkt-lines of a side-band-64k answer, split by band: `(band 1, band 3)`
/// payloads concatenated, keepalives counted. Panics on a plain line — a
/// side-band client reads every byte through its demuxer.
fn demux(answer: &[u8]) -> (Vec<u8>, Vec<u8>, usize) {
    let (mut pack, mut errors, mut keepalives) = (Vec::new(), Vec::new(), 0);
    let mut at = answer;
    loop {
        let len = usize::from_str_radix(std::str::from_utf8(&at[..4]).unwrap(), 16).unwrap();
        if len == 0 {
            assert_eq!(at.len(), 4, "the flush ends the answer");
            return (pack, errors, keepalives);
        }
        let payload = &at[5..len];
        match at[4] {
            1 if payload.is_empty() => keepalives += 1,
            1 => pack.extend_from_slice(payload),
            3 => errors.extend_from_slice(payload),
            band => panic!("unexpected band {band}"),
        }
        at = &at[len..];
    }
}

/// A side-band client reads its refusal on band 1 — the same report, muxed,
/// because once the client asked for side-band a plain line is a protocol
/// error to it.
#[tokio::test]
async fn a_side_band_push_reads_its_report_on_band_1() {
    let (_directory, router) = application();
    let request = Request::builder()
        .method("POST")
        .uri("/lab/git-receive-pack")
        .body(Body::from(unsigned_push_body_with_caps(
            "report-status side-band-64k",
        )))
        .unwrap();
    let response = router.oneshot(authenticated(request)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let (report, errors, keepalives) = demux(&bytes);
    let report = String::from_utf8_lossy(&report);
    assert!(report.contains("unpack ok"), "{report}");
    assert!(report.contains("ng refs/heads/main "), "{report}");
    assert!(errors.is_empty() && keepalives == 0);
}

/// A PUSH'S ANSWER STARTS BEFORE ITS BLOCK. The node holds a blob-bearing
/// submit until the pack has fanned out to every validator and the block
/// commits — minutes for a repository-sized pack — while every hop between
/// git and this service (the browser Gateway's 60 s silent-upstream ceiling
/// above all) cuts an exchange that goes quiet. With side-band-64k the head
/// goes out as soon as the push is admitted and keepalives ride band 1 until
/// the fate lands, so the exchange is bounded on progress, never on the size
/// of the pack (#2791). Here the node never answers at all: the keepalives
/// outlast the ceiling many times over, and the fate arrives as a band-3
/// error — nobody said no, so no ref is reported rejected.
#[tokio::test(start_paused = true)]
async fn a_push_held_past_the_gateway_ceiling_keeps_its_answer_alive() {
    use futures::StreamExt as _;
    const GATEWAY_SILENCE_CEILING: std::time::Duration = std::time::Duration::from_secs(60);
    // a node that accepts the connection and never answers.
    let node = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node_url = format!("http://{}", node.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = node.accept().await {
            held.push(socket);
        }
    });
    let (_directory, router) = application_on("chain-a", &node_url);
    let response = router
        .oneshot(authenticated(
            Request::builder()
                .method("POST")
                .uri("/lab/git-receive-pack")
                .body(Body::from(signed_push_body(
                    "report-status side-band-64k",
                    b"PACK",
                )))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the head is out");
    let mut answer = response.into_body().into_data_stream();
    let mut lines = Vec::new();
    loop {
        let next = answer.next();
        tokio::pin!(next);
        assert!(
            futures::poll!(&mut next).is_pending(),
            "nothing is due before the keepalive interval"
        );
        tokio::time::advance(GIT_KEEPALIVE_INTERVAL).await;
        let line = next.await.unwrap().unwrap();
        let is_keepalive = &line[..] == b"0005\x01";
        lines.extend_from_slice(&line);
        if !is_keepalive {
            break;
        }
    }
    while let Some(chunk) = answer.next().await {
        lines.extend_from_slice(&chunk.unwrap());
    }
    let (report, errors, keepalives) = demux(&lines);
    assert!(
        GIT_KEEPALIVE_INTERVAL * keepalives as u32 > GATEWAY_SILENCE_CEILING,
        "{keepalives} keepalives do not outlast the gateway ceiling"
    );
    assert!(report.is_empty(), "no ref was reported on: {report:?}");
    assert!(!errors.is_empty(), "the unresolved fate rides band 3");
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
    let tree = repo
        .find_tree(repo.index().unwrap().write_tree().unwrap())
        .unwrap();
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

/// A PINNED COMMIT IS FETCHABLE BY SHA. upload-pack advertises
/// `allow-reachable-sha1-in-want` — without it stock git never sends a want
/// for a commit that is not a tip — and a want no ref reaches is refused as a
/// git `ERR` line naming the commit and the repo, which git prints as
/// `fatal: remote error: …` (an HTTP error status would reach the user as a
/// bare "HTTP 400").
#[tokio::test]
async fn upload_pack_admits_reachable_wants_and_refuses_the_rest_by_name() {
    let (directory, router) = application();
    seed_repo(directory.path(), "lab", &["dev"]);
    let body = advertisement(router.clone(), "lab").await;
    assert!(body.contains("allow-reachable-sha1-in-want"), "{body}");

    let unknown = "11".repeat(20);
    let request_body = format!(
        "{}0000{}",
        pkt(&format!("want {unknown} side-band-64k\n")),
        pkt("done\n")
    );
    let response = router
        .oneshot(authenticated(
            Request::builder()
                .method("POST")
                .uri("/lab/git-upload-pack")
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        pkt(&format!(
            "ERR commit {unknown} is not reachable from any ref of lab\n"
        ))
    );
}

/// THE BUG (#2712): the answer's head went out only once the whole pack was
/// built, so a repository whose pack took longer than the Gateway's ceiling on
/// a silent publisher could not be cloned at all. The head now leaves on
/// admission, a builder still at work keeps the answer alive with git's
/// keepalive, and the pack that follows installs. "Slow" is a builder held
/// past the ceiling on the paused clock, never a sleep.
#[tokio::test(start_paused = true)]
async fn a_pack_slower_than_the_gateway_ceiling_still_clones() {
    use futures::StreamExt as _;
    // noded's `PROXY_REPLY_TIMEOUT`; the node asserts the keepalive under it.
    const GATEWAY_SILENCE_CEILING: std::time::Duration = std::time::Duration::from_secs(60);
    let (directory, router) = application();
    let repo_dir = directory.path().join("slow");
    let repo = git2::Repository::init_bare(&repo_dir).unwrap();
    let blob = repo.blob(b"slow\n").unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("slow.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::new("t", "t@t", &git2::Time::new(0, 0)).unwrap();
    let head = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "slow",
            &tree,
            &[],
        )
        .unwrap();
    let (release, gate) = std::sync::mpsc::channel();
    crate::git_http::pack_gate::GATES
        .lock()
        .unwrap()
        .push((repo_dir, gate));

    let request = format!(
        "{}0000{}",
        pkt(&format!("want {head} multi_ack_detailed side-band-64k\n")),
        pkt("done\n")
    );
    let response = router
        .oneshot(authenticated(
            Request::builder()
                .method("POST")
                .uri("/slow/git-upload-pack")
                .body(Body::from(request))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the head is out");
    let mut answer = response.into_body().into_data_stream();
    assert_eq!(
        answer.next().await.unwrap().unwrap(),
        pkt("NAK\n").as_bytes()
    );
    let mut held = std::time::Duration::ZERO;
    while held <= GATEWAY_SILENCE_CEILING {
        let next = answer.next();
        tokio::pin!(next);
        assert!(
            futures::poll!(&mut next).is_pending(),
            "the builder is held, so nothing is due yet"
        );
        tokio::time::advance(GIT_KEEPALIVE_INTERVAL).await;
        assert_eq!(
            next.await.unwrap().unwrap(),
            &b"0005\x01"[..],
            "a keepalive"
        );
        held += GIT_KEEPALIVE_INTERVAL;
    }

    release.send(()).unwrap();
    let mut rest = Vec::new();
    while let Some(chunk) = answer.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    let mut pack = Vec::new();
    let mut at = rest.as_slice();
    loop {
        let len = usize::from_str_radix(std::str::from_utf8(&at[..4]).unwrap(), 16).unwrap();
        if len == 0 {
            assert_eq!(at.len(), 4, "the flush ends the answer");
            break;
        }
        assert_eq!(at[4], 0x01, "pack bytes ride band 1 only");
        pack.extend_from_slice(&at[5..len]);
        at = &at[len..];
    }
    let clone = tempfile::tempdir().unwrap();
    let cloned = git2::Repository::init_bare(clone.path()).unwrap();
    let odb = cloned.odb().unwrap();
    let mut indexer = odb.packwriter().unwrap();
    std::io::Write::write_all(&mut indexer, &pack).unwrap();
    indexer.commit().unwrap();
    assert_eq!(cloned.find_commit(head).unwrap().message(), Some("slow"));
}
