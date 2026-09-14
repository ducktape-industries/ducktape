//! End-to-end: the whole custody path in one process, over real HTTP —
//! attest → seal credential → handshake → proxied call → credential swap →
//! reply. The gateway runs a testkit quoter (minted SNP chain) and the client
//! side verifies it through the REAL `airlock::verify` path under the
//! enclave's own roots. Run with
//! `cargo test -p airlock --features server,client,testkit`.
#![cfg(all(feature = "server", feature = "client", feature = "testkit"))]

use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tokio::net::TcpListener;

use airlock::attest::{self, Measurement};
use airlock::client::Gateway;
use airlock::seal::SealKeypair;
use airlock::server::{self, AttestMode, GatewayConfig};
use airlock::testkit::SnpTestEnclave;
use airlock::wire::{
    AttestationResponse, CredentialKind, CredentialPayload, SessionRequest, WorkRef,
};

/// 48-byte measurement (all 0x11) shared by the gateway and the verifying client.
fn measurement() -> Measurement {
    Measurement([0x11; attest::MRTD_LEN])
}

fn enclave() -> Arc<SnpTestEnclave> {
    Arc::new(SnpTestEnclave::new(&measurement()).unwrap())
}

/// A mock Anthropic upstream. `/oauth/token` mints `acc-1`; `/v1/messages`
/// accepts ONLY `Bearer acc-1` — so a 200 proves the gateway swapped the
/// session token for the real access token.
#[derive(Default)]
struct MockUpstream {
    n: Mutex<u64>,
}

async fn oauth(State(st): State<Arc<MockUpstream>>) -> Json<serde_json::Value> {
    let mut n = st.n.lock().unwrap();
    *n += 1;
    Json(json!({ "access_token": format!("acc-{n}"), "refresh_token": format!("ref-{n}"), "expires_in": 3600 }))
}

async fn messages(State(st): State<Arc<MockUpstream>>, headers: HeaderMap) -> Response {
    let want = format!("Bearer acc-{}", *st.n.lock().unwrap());
    let got = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if got != want {
        return (StatusCode::UNAUTHORIZED, format!("want {want:?} got {got:?}")).into_response();
    }
    let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"AIRLOCK-OK\"}}\n\n\
               event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    ([("content-type", "text/event-stream")], sse).into_response()
}

/// Bind a listener on an ephemeral port and serve `app`; returns the base URL.
/// `bind` makes the socket accept into the backlog immediately, so a client may
/// connect right away — no readiness sleep needed.
async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn boot_gateway(upstream: &str, enclave: &Arc<SnpTestEnclave>) -> String {
    let (app, vendor) = server::build_with_quoter(
        GatewayConfig {
            attest: AttestMode::Tsm("snp".into()),
            seal_keypair: None,
            anthropic_base: upstream.into(),
            openai_base: String::new(),
            oauth_token_url: format!("{upstream}/oauth/token"),
            oauth_client_id: "test-client".into(),
            session_ttl_secs: 3600,
            max_requests: 100,
            sign: None,
        },
        "snp",
        enclave.quoter(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(vendor, "snp");
    spawn(app).await
}

async fn boot_upstream() -> String {
    let app = Router::new()
        .route("/oauth/token", post(oauth))
        .route("/v1/messages", post(messages))
        .with_state(Arc::new(MockUpstream::default()));
    spawn(app).await
}

/// Like [`boot_upstream`], but `/v1/messages` answers ANY method — used to
/// prove a bodyless request (a GET) reaches the upstream at all, which a
/// POST-only mock can't distinguish from "the method itself was rejected".
async fn boot_upstream_any_method() -> String {
    let app = Router::new()
        .route("/oauth/token", post(oauth))
        .route("/v1/messages", any(messages))
        .with_state(Arc::new(MockUpstream::default()));
    spawn(app).await
}

/// The verified `seal_pk` the client trusts only because it checked the quote
/// — through the real SNP verifier, under the enclave's own roots.
async fn attested_seal_pk(gw: &Gateway, enclave: &Arc<SnpTestEnclave>) -> [u8; 32] {
    let (quote, vendor) = gw.fetch_quote().await.unwrap();
    assert_eq!(vendor, "snp");
    let rd = airlock::verify::verify_quote(&quote, &measurement(), &enclave.roots())
        .await
        .unwrap();
    attest::split_report_data(&rd).0
}

#[tokio::test]
async fn full_custody_path_swaps_session_token_for_the_credential() {
    let upstream = boot_upstream().await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    // Credential Provider: verify the quote, then seal + upload the refresh token.
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Refresh { refresh_token: "ref-seed".into(), access_token: String::new(), expires_at: 0 },
    )
    .await
    .unwrap();

    // Computation Provider: handshake for a scoped token, then a proxied call.
    let token = gw
        .open_session(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-5","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(body.contains("AIRLOCK-OK"), "reply should stream back through the gateway: {body}");
}

#[tokio::test]
async fn proxy_rejects_a_request_without_a_valid_session_token() {
    let upstream = boot_upstream().await;
    let gateway_url = boot_gateway(&upstream, &enclave()).await;

    // A bare bearer that is not a gateway-issued session token is refused before
    // any credential is spent.
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth("not-a-real-session-token")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_forged_gateway_cannot_mint_a_token_the_client_opens() {
    // If the client handshakes against a DIFFERENT enclave's seal_pk than the one
    // that answered, open_token fails — the session binds to the attested key.
    let upstream = boot_upstream().await;
    let gateway_url = boot_gateway(&upstream, &enclave()).await;
    let gw = Gateway::local(gateway_url);

    let wrong_seal_pk = [0x42u8; 32]; // not the gateway's attested key
    let err = gw.open_session(&wrong_seal_pk, "test-sub", &WorkRef::Direct).await;
    assert!(err.is_err(), "a token derived against the wrong seal_pk must not open");
}

#[tokio::test]
async fn sealed_session_carries_only_ciphertext_and_round_trips_plaintext() {
    use airlock::bodyseal::{self, OpenedItem};
    use std::sync::atomic::{AtomicBool, Ordering};

    // Upstream asserts it receives the EXACT plaintext body — proving the
    // enclave (and nowhere else) unsealed the request.
    let saw_plaintext = Arc::new(AtomicBool::new(false));
    let seen = saw_plaintext.clone();
    let app = Router::new()
        .route("/oauth/token", post(oauth))
        .route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let seen = seen.clone();
                async move {
                    let got = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
                    if !got.starts_with("Bearer acc-") {
                        return (StatusCode::UNAUTHORIZED, format!("got {got:?}")).into_response();
                    }
                    if body.as_ref() != br#"{"secret":"prompt"}"# {
                        return (StatusCode::BAD_REQUEST, "upstream saw non-plaintext body")
                            .into_response();
                    }
                    seen.store(true, Ordering::SeqCst);
                    ([("content-type", "text/event-stream")], "data: SEALED-OK\n\n")
                        .into_response()
                }
            }),
        )
        .with_state(Arc::new(MockUpstream::default()));
    // The state is unused by these closures but keeps Router typing uniform.
    let upstream = spawn(app).await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Refresh { refresh_token: "ref-seed".into(), access_token: String::new(), expires_at: 0 },
    )
    .await
    .unwrap();

    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();
    let aad = bodyseal::request_aad("POST", "/v1/messages");
    let sealed_body = bodyseal::seal_request(&keys, &aad, br#"{"secret":"prompt"}"#);
    assert!(
        !sealed_body.windows(6).any(|w| w == b"prompt"),
        "the wire body must not contain the plaintext"
    );
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .header("content-type", "application/json")
        .body(sealed_body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream",
        "the outer response is opaque ciphertext"
    );
    let wire = resp.bytes().await.unwrap();
    assert!(
        !wire.windows(9).any(|w| w == b"SEALED-OK"),
        "the wire response must not contain the plaintext"
    );
    let mut opener = bodyseal::StreamOpener::new(&keys, &bodyseal::request_binding(&sealed_body));
    let items = opener.feed(&wire).unwrap();
    assert!(opener.finished(), "the sealed stream must end with the Final marker");
    let plaintext: Vec<u8> = items
        .iter()
        .filter_map(|item| match item {
            OpenedItem::Data(data) => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(String::from_utf8(plaintext).unwrap().contains("SEALED-OK"));
    assert!(matches!(&items[0], OpenedItem::Head(ct) if ct == "text/event-stream"));
    assert!(saw_plaintext.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_sealed_session_refuses_a_plaintext_body() {
    let upstream = boot_upstream().await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Refresh { refresh_token: "ref-seed".into(), access_token: String::new(), expires_at: 0 },
    )
    .await
    .unwrap();
    let (token, _keys) = gw
        .open_session_sealed(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();

    // A plaintext body on a sealed session = what a bearer thief can produce.
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(r#"{"stolen":"bearer"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a stolen bearer without the body key must be useless"
    );
}

/// **A sealed session seals EVERY request, bodyless ones included.** A bare
/// GET carrying only the stolen bearer — the exact thing a path host or a
/// browser-gateway hop can observe — must not spend the credential; a GET
/// whose (empty) body is properly sealed must.
#[tokio::test]
async fn a_sealed_session_requires_a_sealed_body_even_on_a_bodyless_get() {
    use airlock::bodyseal;

    let upstream = boot_upstream_any_method().await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Refresh {
            refresh_token: "ref-seed".into(),
            access_token: String::new(),
            expires_at: 0,
        },
    )
    .await
    .unwrap();
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();

    // A bare GET with only the bearer and no seal header — a stolen bearer
    // alone can produce exactly this — must be refused.
    let bare = reqwest::Client::new()
        .get(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        bare.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a stolen bearer must buy nothing, even on a bodyless GET"
    );

    // The same GET, with an empty plaintext sealed under the handshake keys
    // (the AEAD tag alone, no payload), must be admitted — the caller proved
    // possession of the keys, which a bearer thief lacks.
    let aad = bodyseal::request_aad("GET", "/v1/messages");
    let sealed_empty = bodyseal::seal_request(&keys, &aad, b"");
    let sealed = reqwest::Client::new()
        .get(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .body(sealed_empty)
        .send()
        .await
        .unwrap();
    assert_eq!(
        sealed.status(),
        reqwest::StatusCode::OK,
        "a sealed empty body must be admitted like any other sealed request"
    );
}

/// The gateway's own `DefaultBodyLimit` must match the broker's
/// `MAX_REQUEST_BYTES`, not axum's implicit 2 MiB default — a sealed body
/// between the two (3 MiB) must reach the upstream, not 413 at the router.
#[tokio::test]
async fn a_3mib_sealed_body_reaches_proxy_inner() {
    use airlock::bodyseal;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let plaintext = vec![7u8; 3 * 1024 * 1024];
    let seen_len = Arc::new(AtomicUsize::new(0));
    let seen = seen_len.clone();
    let app = Router::new()
        .route("/oauth/token", post(oauth))
        .route(
            "/v1/messages",
            post(move |body: axum::body::Bytes| {
                let seen = seen.clone();
                async move {
                    seen.store(body.len(), Ordering::SeqCst);
                    ([("content-type", "text/event-stream")], "data: OK\n\n").into_response()
                }
            }),
        )
        // The MOCK upstream is a plain axum router and would otherwise apply
        // the SAME 2 MiB default this test exists to prove the gateway no
        // longer imposes — raise it so a 413 here can only mean the gateway
        // itself rejected the body.
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(Arc::new(MockUpstream::default()));
    let upstream = spawn(app).await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Refresh {
            refresh_token: "ref-seed".into(),
            access_token: String::new(),
            expires_at: 0,
        },
    )
    .await
    .unwrap();
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();
    let aad = bodyseal::request_aad("POST", "/v1/messages");
    let sealed_body = bodyseal::seal_request(&keys, &aad, &plaintext);

    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .body(sealed_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "a 3 MiB sealed body must reach the upstream, not 413 at axum's 2 MiB default"
    );
    assert_eq!(seen_len.load(Ordering::SeqCst), plaintext.len());
}

#[tokio::test]
async fn build_seeded_uses_the_initial_credential_without_upload() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // The credential is seeded at build (the node-embed path) — no /credential
    // upload, and a static bearer must never trigger an OAuth refresh.
    let oauth_hits = Arc::new(AtomicU64::new(0));
    let oh = oauth_hits.clone();
    let app = Router::new()
        .route(
            "/oauth/token",
            post(move || {
                let oh = oh.clone();
                async move {
                    oh.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "no oauth for a seeded bearer").into_response()
                }
            }),
        )
        .route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                let got = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
                if got != "Bearer seeded-tok" {
                    return (StatusCode::UNAUTHORIZED, format!("got {got:?}")).into_response();
                }
                ([("content-type", "text/event-stream")], "data: AIRLOCK-OK\n\n").into_response()
            }),
        );
    let upstream = spawn(app).await;

    let enclave = enclave();
    let (router, vendor) = server::build_with_quoter(
        GatewayConfig {
            attest: AttestMode::Tsm("snp".into()),
            seal_keypair: None,
            anthropic_base: upstream.clone(),
            openai_base: String::new(),
            oauth_token_url: format!("{upstream}/oauth/token"),
            oauth_client_id: "test-client".into(),
            session_ttl_secs: 3600,
            max_requests: 100,
            sign: None,
        },
        "snp",
        enclave.quoter(),
        vec![(
            "sub".into(),
            CredentialKind::Claude,
            CredentialPayload::Bearer { access_token: "seeded-tok".into() },
        )],
    )
    .unwrap();
    assert_eq!(vendor, "snp");
    let gateway_url = spawn(router).await;
    let gw = Gateway::local(gateway_url.clone());

    // NO upload_sealed_credential — the credential was seeded at build.
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    let token = gw
        .open_session(&seal_pk, "sub", &WorkRef::Direct)
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp.text().await.unwrap().contains("AIRLOCK-OK"));
    assert_eq!(oauth_hits.load(Ordering::SeqCst), 0, "a seeded bearer must not OAuth-refresh");
}

#[tokio::test]
async fn static_bearer_credential_is_used_without_any_oauth_refresh() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // An upstream that FAILS if /oauth/token is ever hit, and accepts only the
    // exact static bearer on /v1/messages — proving a sealed Bearer is used as-is,
    // never refreshed (so a live subscription's token chain is not rotated).
    let oauth_hits = Arc::new(AtomicU64::new(0));
    let oh = oauth_hits.clone();
    let app = Router::new()
        .route(
            "/oauth/token",
            post(move || {
                let oh = oh.clone();
                async move {
                    oh.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "oauth must not be called for a static bearer")
                        .into_response()
                }
            }),
        )
        .route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                let got = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
                if got != "Bearer static-access-xyz" {
                    return (StatusCode::UNAUTHORIZED, format!("got {got:?}")).into_response();
                }
                ([("content-type", "text/event-stream")], "data: AIRLOCK-OK\n\n").into_response()
            }),
        );
    let upstream = spawn(app).await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url.clone());

    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &airlock::wire::CredentialPayload::Bearer { access_token: "static-access-xyz".into() },
    )
    .await
    .unwrap();

    let token = gw
        .open_session(&seal_pk, "test-sub", &WorkRef::Direct)
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(body.contains("AIRLOCK-OK"), "static bearer should reach upstream: {body}");
    assert_eq!(
        oauth_hits.load(Ordering::SeqCst),
        0,
        "a static bearer must NOT trigger an OAuth refresh (no rotation)"
    );
}

// -------- self-host mode: named multi-credential store + per-kind upstream --------

/// A self-host `GatewayConfig` (no TEE, empty quote). `seal_keypair` is injected
/// so the test client can handshake against a known seal_pk — the self-host trust
/// anchor is the pinned key, not a verified quote.
fn self_host_cfg(
    seal_keypair: Option<SealKeypair>,
    anthropic_base: String,
    openai_base: String,
) -> GatewayConfig {
    GatewayConfig {
        attest: AttestMode::SelfHost,
        seal_keypair,
        anthropic_base,
        openai_base,
        oauth_token_url: String::new(),
        oauth_client_id: String::new(),
        session_ttl_secs: 3600,
        max_requests: 100,
        sign: None,
    }
}

/// An upstream that echoes back the `Authorization` header the gateway sent, so a
/// round-trip reveals exactly which credential's token the proxy planted.
///
/// This is the ONE fixture that stands in for BOTH vendors, so it must serve
/// every shape `server::upstream_path` can produce from the caller's
/// `/v1/messages` — claude passes through, codex has its `/v1` stripped (the
/// ChatGPT backend serves `/responses` under `/backend-api/codex`, with no
/// `/v1`). A fixture that knows only one vendor's shape does not fail loudly: it
/// 404s, the echo comes back EMPTY, and the assertion blames the credential.
/// That is how this went red and stayed red — add the new shape here when a
/// third credential kind lands, not after the next bisect.
async fn boot_echo_upstream() -> String {
    async fn echo(headers: HeaderMap) -> Response {
        let got = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        ([("content-type", "text/plain")], got).into_response()
    }
    spawn(
        Router::new()
            .route("/v1/messages", post(echo)) // claude: pass-through
            .route("/messages", post(echo)), // codex: `/v1` stripped
    )
    .await
}

/// Open a session for `name` and POST once; return the `Authorization` header the
/// upstream saw (the echo upstream reflects it in the body).
async fn round_trip_via(gw: &Gateway, gateway_url: &str, seal_pk: &[u8; 32], name: &str) -> String {
    let token = gw
        .open_session(seal_pk, name, &WorkRef::Direct)
        .await
        .unwrap();
    reqwest::Client::new()
        .post(format!("{gateway_url}/v1/messages"))
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn sessions_route_to_the_named_credential() {
    let upstream = boot_echo_upstream().await;
    let kp = SealKeypair::generate();
    let seal_pk = kp.public_bytes();
    let (app, vendor) = server::build_seeded(
        self_host_cfg(Some(kp), upstream.clone(), String::new()),
        vec![
            ("a".into(), CredentialKind::Claude, CredentialPayload::Bearer {
                access_token: "tok-a".into(),
            }),
            ("b".into(), CredentialKind::Claude, CredentialPayload::Bearer {
                access_token: "tok-b".into(),
            }),
        ],
    )
    .unwrap();
    assert_eq!(vendor, "self-host");
    let gateway_url = spawn(app).await;
    let gw = Gateway::local(gateway_url.clone());

    let seen_a = round_trip_via(&gw, &gateway_url, &seal_pk, "a").await;
    let seen_b = round_trip_via(&gw, &gateway_url, &seal_pk, "b").await;
    assert_eq!(seen_a, "Bearer tok-a");
    assert_eq!(seen_b, "Bearer tok-b");
}

/// `ducktape user cred remove` deletes the store dir and commits the tombstone,
/// after which the store loader reports the name ABSENT. Every session already
/// holding a token for it must stop drawing on the credential right then — the
/// gateway used to keep the parsed entry (live access + refresh token) and go
/// on proxying to the vendor for the rest of an hour's TTL and 4096 requests,
/// so the operator watched the removal "work" while the credential was still
/// being spent.
#[tokio::test]
async fn a_removed_credential_stops_serving_the_sessions_already_open_on_it() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let upstream = boot_echo_upstream().await;
    let kp = SealKeypair::generate();
    let seal_pk = kp.public_bytes();
    // The store, as the gateway sees it: holding the credential until the
    // operator removes it, and answering `Absent` from then on.
    let removed = Arc::new(AtomicBool::new(false));
    let store = removed.clone();
    let reload: airlock::server::ReloadCredential = Arc::new(move |_name: &str| {
        match store.load(Ordering::SeqCst) {
            true => airlock::server::StoreLoad::Absent,
            false => airlock::server::StoreLoad::Unchanged,
        }
    });
    let (app, _) = server::build_self_host_reloadable(
        self_host_cfg(Some(kp), upstream.clone(), String::new()),
        vec![("a".into(), CredentialKind::Claude, CredentialPayload::Bearer {
            access_token: "tok-a".into(),
        })],
        None,
        reload,
    )
    .unwrap();
    let gateway_url = spawn(app).await;
    let gw = Gateway::local(gateway_url.clone());

    let token = gw.open_session(&seal_pk, "a", &WorkRef::Direct).await.unwrap();
    let proxied = || {
        reqwest::Client::new()
            .post(format!("{gateway_url}/v1/messages"))
            .bearer_auth(&token)
            .body("{}")
            .send()
    };
    let before = proxied().await.unwrap();
    assert_eq!(before.status(), reqwest::StatusCode::OK);
    assert_eq!(before.text().await.unwrap(), "Bearer tok-a");

    removed.store(true, Ordering::SeqCst);

    // The same session token, unexpired and with budget left over.
    let after = proxied().await.unwrap();
    assert_eq!(
        after.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a removed credential must not be spendable by an outstanding session"
    );
}

#[tokio::test]
async fn unknown_credential_name_is_refused_at_session_open() {
    let (app, _) =
        server::build_seeded(self_host_cfg(None, String::new(), String::new()), vec![]).unwrap();
    let gateway_url = spawn(app).await;
    // The name check precedes the handshake, so the eph field is unread here.
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}/session"))
        .json(&SessionRequest {
            sub: "missing".into(),
            client_eph_pk_b64: "AAAA".into(),
            body_seal: false,
            work: WorkRef::Direct,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn self_host_attestation_reports_no_quote() {
    let (app, _) =
        server::build_seeded(self_host_cfg(None, String::new(), String::new()), vec![]).unwrap();
    let gateway_url = spawn(app).await;
    let att: AttestationResponse = reqwest::Client::new()
        .get(format!("{gateway_url}/attestation"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(att.vendor, "self-host");
    assert!(att.quote_b64.is_empty());
}

#[tokio::test]
async fn codex_credential_proxies_to_the_openai_upstream() {
    let openai = boot_echo_upstream().await;
    let kp = SealKeypair::generate();
    let seal_pk = kp.public_bytes();
    // anthropic_base is a bogus URL: a codex session must never touch it.
    let (app, _) = server::build_seeded(
        self_host_cfg(Some(kp), "http://anthropic.invalid".into(), openai.clone()),
        vec![("cx".into(), CredentialKind::Codex, CredentialPayload::Bearer {
            access_token: "tok-codex".into(),
        })],
    )
    .unwrap();
    let gateway_url = spawn(app).await;
    let gw = Gateway::local(gateway_url.clone());

    let seen = round_trip_via(&gw, &gateway_url, &seal_pk, "cx").await;
    assert_eq!(seen, "Bearer tok-codex", "a codex session hits the openai upstream with its bearer");
}

// -------- the co-hosted lending gate --------

/// The saga id the stub gate below treats as work the caller may delegate on.
/// Nothing in THIS crate can resolve a saga — that decision belongs to the node
/// (`bin/node/src/airlock.rs`), which reads it out of committed state. What the
/// stub stands for here is only that the pointer ARRIVES.
const DELEGABLE: &str = "sched\u{1f}delegable";

/// The injected gate: only the node `granted` may draw on the credential —
/// the node's committed-state resolution, stubbed here. `wedged` stands in for
/// a node that did not answer at all. A caller presenting [`DELEGABLE`] is
/// admitted whoever it is, standing in for a real lender resolving that saga
/// and finding a granted submitter.
fn stub_grant_check() -> airlock::server::GrantCheck {
    std::sync::Arc::new(|question: airlock::server::GrantQuestion| {
        Box::pin(async move {
            let delegated = question.work
                == (WorkRef::Saga {
                    saga_id: DELEGABLE.into(),
                });
            if delegated {
                return airlock::server::GrantAnswer::Granted;
            }
            match question.caller_node.as_slice() {
                b"granted" => airlock::server::GrantAnswer::Granted,
                b"wedged" => airlock::server::GrantAnswer::Undetermined,
                _other => airlock::server::GrantAnswer::Refused,
            }
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = airlock::server::GrantAnswer> + Send>,
            >
    })
}

/// The wire half of delegation, and the ONLY half this crate owns: the pointer
/// a session presents reaches the injected authority intact, and it is what the
/// authority answered on — the same caller, the same credential, refused on one
/// pointer and admitted on the other.
///
/// Which pointers a real lender admits is decided in `bin/node/src/airlock.rs`
/// against committed state, and is tested there and on two live nodes.
#[tokio::test]
async fn the_work_pointer_a_session_presents_reaches_the_grant_gate() {
    let upstream = boot_echo_upstream().await;
    let secret = SealKeypair::generate().secret_bytes();
    let seal_pk = SealKeypair::from_secret_bytes(secret).public_bytes();
    // one ungranted caller, one lender, two sessions.
    let url = boot_lender_behind_proxy(&upstream, secret, b"stranger").await;
    let gw = Gateway::local(url);

    let direct = gw.open_session(&seal_pk, "a", &WorkRef::Direct).await;
    assert!(
        direct.unwrap_err().to_string().contains("403"),
        "an ungranted caller with nothing to point at is refused"
    );
    gw.open_session(
        &seal_pk,
        "a",
        &WorkRef::Saga {
            saga_id: DELEGABLE.into(),
        },
    )
    .await
    .expect("the same caller opens when its pointer is one the authority admits");
}

/// A grant-gated self-host lender, reached the ONLY way production reaches one:
/// through the node's gateway proxy, which stamps the node it verified for the
/// caller. `verified_caller` is what that proxy vouched for — the test chooses
/// it because a real caller cannot.
///
/// `seal_secret` is passed in rather than generated so several differently-
/// vouched gateways share one seal_pk, exactly as one lender serving several
/// borrowers does.
async fn boot_lender_behind_proxy(
    upstream: &str,
    seal_secret: [u8; 32],
    verified_caller: &[u8],
) -> String {
    let (app, _) = server::build_seeded_gated(
        self_host_cfg(
            Some(SealKeypair::from_secret_bytes(seal_secret)),
            upstream.to_string(),
            String::new(),
        ),
        vec![(
            "a".into(),
            CredentialKind::Claude,
            CredentialPayload::Bearer { access_token: "tok-a".into() },
        )],
        Some(stub_grant_check()),
    )
    .unwrap();
    spawn(airlock::testkit::behind_gateway_proxy(app, verified_caller)).await
}

#[tokio::test]
async fn grant_gate_admits_what_the_authority_grants_and_refuses_the_rest() {
    let upstream = boot_echo_upstream().await;
    let secret = SealKeypair::generate().secret_bytes();
    let seal_pk = SealKeypair::from_secret_bytes(secret).public_bytes();

    // Granted caller: the session opens and the round-trip carries the real token.
    let url = boot_lender_behind_proxy(&upstream, secret, b"granted").await;
    let gw = Gateway::local(url.clone());
    let token =
        gw.open_session(&seal_pk, "a", &WorkRef::Direct).await.expect("granted session opens");
    let seen = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(seen, "Bearer tok-a");

    // Ungranted caller: refused at session open with 403. The caller really IS
    // `stranger` here — the proxy said so — so this is the grant refusal and not
    // an identity one.
    let url = boot_lender_behind_proxy(&upstream, secret, b"stranger").await;
    let gw = Gateway::local(url);
    let err = gw.open_session(&seal_pk, "a", &WorkRef::Direct).await.unwrap_err();
    assert!(err.to_string().contains("403"), "an ungranted caller must 403: {err}");

    // The gate could not ASK its authority. That is NOT a refusal: a 403 sends
    // the borrower's operator to add a grant that already exists, so the one
    // answer that carries no information gets its own 503 instead.
    let url = boot_lender_behind_proxy(&upstream, secret, b"wedged").await;
    let gw = Gateway::local(url);
    let err = gw
        .open_session(&seal_pk, "a", &WorkRef::Direct)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("503"),
        "an undetermined grant must 503, never 403: {err}"
    );
}

/// THE ATTACK, and why it is now a fact about the TYPES rather than a runtime
/// refusal. A member the network admitted, granted nothing, reads the lender's
/// PUBLIC credential record — `owner_account` is a plain field of it, and the
/// borrower has to read that record anyway to learn `seal_pk` — and tries to open
/// a session as the owner.
///
/// It cannot construct the request. `SessionRequest` has no account field and the
/// client exposes no call that takes one, so this asserts the only thing left to
/// assert: a hand-built request that tries to smuggle the owner's account in is a
/// DECODE error, not a session. `deny_unknown_fields` is what makes that true, so
/// re-adding the field in any form fails here.
#[tokio::test]
async fn a_session_request_cannot_name_an_account_at_all() {
    let upstream = boot_echo_upstream().await;
    let secret = SealKeypair::generate().secret_bytes();
    // the proxy vouched for `stranger`, who is granted nothing.
    let url = boot_lender_behind_proxy(&upstream, secret, b"stranger").await;

    let refusal = reqwest::Client::new()
        .post(format!("{url}/session"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"sub":"a","client_eph_pk_b64":"AAAA","body_seal":false,"account_b64":"{}"}}"#,
            BASE64.encode(b"granted")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refusal.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "an account on a session request is an unknown field, not an authorization input"
    );

    // and the well-formed request from the same caller is refused on the node
    // the transport vouched for, which is the only identity that counts.
    let gw = Gateway::local(url);
    let err = gw
        .open_session(
            &SealKeypair::from_secret_bytes(secret).public_bytes(),
            "a",
            &WorkRef::Direct,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("403"), "an ungranted caller must 403: {err}");
}

/// The same gate from the other direction: a caller that reached the listener
/// WITHOUT the node's proxy has no verified identity at all, so a lending
/// gateway must refuse it outright rather than fall back to the claim. A
/// same-box process dialling the loopback port is exactly that caller.
#[tokio::test]
async fn a_session_no_proxy_vouched_for_is_refused() {
    let upstream = boot_echo_upstream().await;
    let (app, _) = server::build_seeded_gated(
        self_host_cfg(Some(SealKeypair::generate()), upstream, String::new()),
        vec![(
            "a".into(),
            CredentialKind::Claude,
            CredentialPayload::Bearer { access_token: "tok-a".into() },
        )],
        Some(stub_grant_check()),
    )
    .unwrap();
    // NOT behind the proxy: dialled directly, the way a local process would.
    let url = spawn(app).await;

    let refusal = reqwest::Client::new()
        .post(format!("{url}/session"))
        .json(&SessionRequest {
            sub: "a".into(),
            client_eph_pk_b64: "AAAA".into(),
            body_seal: false,
            work: WorkRef::Direct,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(
        refusal.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an unvouched caller must never open a lending session"
    );
    assert_eq!(refusal.text().await.unwrap(), "caller_node_unverified");
}

/// A client that omits or misspells a field gets a DECODE error, never a grant
/// refusal.
///
/// `account_b64` used to carry `serde(default)`, so an omission silently became
/// `None` and came out the far side as 403 `credential_not_granted` — sending
/// the borrower's operator to add a grant that already existed, which is the
/// exact misdiagnosis the three-state grant taxonomy was built to prevent. The
/// gateway here is one that WOULD admit the caller, so a tolerant decode could
/// only ever be caught as the wrong refusal.
#[tokio::test]
async fn a_malformed_session_request_is_a_decode_error_never_a_grant_refusal() {
    let upstream = boot_echo_upstream().await;
    let secret = SealKeypair::generate().secret_bytes();
    let url = boot_lender_behind_proxy(&upstream, secret, b"granted").await;

    for body in [
        // a required field omitted
        r#"{"sub":"a","client_eph_pk_b64":"AAAA"}"#,
        // and a misspelling, which `deny_unknown_fields` is what catches
        r#"{"sub":"a","client_eph_pk_b64":"AAAA","body_seal":false,"body_sea1":true}"#,
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{url}/session"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "a wire-shape error must be named as one, not as a missing grant: {body}"
        );
    }
}

/// The self-host lender serves NO credential upload. Its router is reachable by
/// any admitted member through the owner's signed `airlock` route, and sealing
/// is not authentication — the seal public key is on chain AND served at
/// `/attestation` — so an upload endpoint there lets any member replace the
/// lender's credential with an attacker-chosen bearer.
///
/// The route must be ABSENT, not present-and-guarded: this asserts on the status
/// axum gives an unrouted path, so re-mounting it fails here even if whatever
/// guard came with it refuses.
#[tokio::test]
async fn the_self_host_lender_serves_no_credential_upload() {
    let kp = SealKeypair::generate();
    let seal_pk = kp.public_bytes();
    let (app, _) = server::build_seeded(
        self_host_cfg(Some(kp), String::new(), String::new()),
        vec![(
            "a".into(),
            CredentialKind::Claude,
            CredentialPayload::Bearer { access_token: "tok-owner".into() },
        )],
    )
    .unwrap();
    let url = spawn(app).await;

    // A perfectly well-formed upload, sealed to the gateway's real key.
    let sealed = airlock::seal::seal(&seal_pk, br#"{"kind":"bearer","access_token":"attacker"}"#);
    let resp = reqwest::Client::new()
        .post(format!("{url}/credential"))
        .json(&airlock::wire::CredentialUpload {
            name: "a".into(),
            kind: CredentialKind::Claude,
            sealed_b64: BASE64.encode(&sealed),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a self-host lender must not route /credential at all"
    );

    // And the credential it already serves is untouched.
    let gw = Gateway::local(url.clone());
    let token = gw
        .open_session(&seal_pk, "a", &WorkRef::Direct)
        .await
        .unwrap();
    let claims = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .body("{}")
        .send()
        .await
        .unwrap();
    // No upstream is configured, so this cannot 200 — what matters is that it
    // got past the token check with the OWNER's credential still in the store.
    assert_ne!(claims.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// The attested build is the one topology where an upload is the only way in:
/// there is a real host-vs-enclave boundary, and the CVM's listener is not
/// something a network member is routed to.
#[tokio::test]
async fn the_attested_gateway_still_accepts_a_sealed_upload() {
    let upstream = boot_upstream().await;
    let enclave = enclave();
    let gateway_url = boot_gateway(&upstream, &enclave).await;
    let gw = Gateway::local(gateway_url);
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    gw.upload_sealed_credential(
        &seal_pk,
        "test-sub",
        CredentialKind::Claude,
        &CredentialPayload::Bearer { access_token: "enclave-tok".into() },
    )
    .await
    .expect("the enclave path keeps its provisioning endpoint");
}

#[test]
fn codex_refresh_seed_is_refused_at_build() {
    let result = server::build_seeded(
        self_host_cfg(None, String::new(), String::new()),
        vec![("cx".into(), CredentialKind::Codex, CredentialPayload::Refresh {
            refresh_token: "r".into(),
            access_token: String::new(),
            expires_at: 0,
        })],
    );
    assert!(result.is_err(), "codex refresh seeds must be rejected (bearer-only lane)");
}
