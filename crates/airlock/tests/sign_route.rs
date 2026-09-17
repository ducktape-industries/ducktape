//! `POST /sign/macos-bundle` end to end: an attested gateway holding a
//! throwaway Developer-ID-shaped identity signs an unsigned fixture
//! `Ducktape.app` posted as a sealed `.tar.zst`, and the sealed reply
//! unpacks to a bundle whose Mach-Os `rcodesign verify` accepts. Apple is
//! the stubbed seam (`sign::Notary::Stub`): notarization itself is only
//! reachable against Apple, with a real identity, from the release lane.
#![cfg(all(feature = "server", feature = "client", feature = "testkit"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

use airlock::attest::{self, Measurement};
use airlock::bodyseal::{self, OpenedItem};
use airlock::client::Gateway;
use airlock::codesign::fixture::{self as cred, Marker};
use airlock::server::{self, AttestMode, GatewayConfig};
use airlock::sign::{self, Notary, StubNotary, fixture};
use airlock::testkit::SnpTestEnclave;
use airlock::wire::{CredentialKind, CredentialPayload, WorkRef};

const ROUTE: &str = "/sign/macos-bundle";

fn measurement() -> Measurement {
    Measurement([0x22; attest::MRTD_LEN])
}

fn enclave() -> Arc<SnpTestEnclave> {
    Arc::new(SnpTestEnclave::new(&measurement()).unwrap())
}

async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn entitlements() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/entitlements.plist")
}

/// An attested gateway with the signing toolchain (`None` mounts no route).
async fn boot_gateway(enclave: &Arc<SnpTestEnclave>, sign: Option<sign::Tools>) -> String {
    let (app, vendor) = server::build_with_quoter(
        GatewayConfig {
            attest: AttestMode::Tsm("snp".into()),
            seal_keypair: None,
            // port 1 never listens: no model call in this lane reaches anything
            anthropic_base: "http://127.0.0.1:1".into(),
            openai_base: String::new(),
            oauth_token_url: String::new(),
            oauth_client_id: String::new(),
            session_ttl_secs: 3600,
            max_requests: 4,
            sign,
        },
        "snp",
        enclave.quoter(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(vendor, "snp");
    spawn(app).await
}

fn tools(work_root: &Path, notary: StubNotary) -> sign::Tools {
    sign::Tools {
        rcodesign: fixture::rcodesign(),
        entitlements: entitlements(),
        work_root: work_root.to_path_buf(),
        notary: Notary::Stub(notary),
    }
}

async fn attested_seal_pk(gw: &Gateway, enclave: &Arc<SnpTestEnclave>) -> [u8; 32] {
    let (quote, vendor) = gw.fetch_quote().await.unwrap();
    assert_eq!(vendor, "snp");
    let rd = airlock::verify::verify_quote(&quote, &measurement(), &enclave.roots())
        .await
        .unwrap();
    attest::split_report_data(&rd).0
}

async fn upload_identity(gw: &Gateway, seal_pk: &[u8; 32], name: &str) {
    gw.upload_sealed_credential(
        seal_pk,
        name,
        CredentialKind::AppleCodesign,
        &CredentialPayload::AppleCodesign {
            p12_b64: cred::p12_b64(cred::TEAM_ID, Marker::DeveloperIdApplication),
            p12_password: cred::P12_PASSWORD.into(),
            api_key_json: cred::api_key_json(),
            team_id: cred::TEAM_ID.into(),
        },
    )
    .await
    .unwrap();
}

/// Post `archive` sealed on a sealed session; returns the status and the
/// raw wire body.
async fn post_sealed(
    gateway_url: &str,
    token: &str,
    keys: &airlock::handshake::SessionKeys,
    archive: &[u8],
) -> (reqwest::StatusCode, Vec<u8>, Vec<u8>) {
    let aad = bodyseal::request_aad("POST", ROUTE);
    let sealed = bodyseal::seal_request(keys, &aad, archive);
    let binding = bodyseal::request_binding(&sealed);
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}{ROUTE}"))
        .bearer_auth(token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .body(sealed)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let wire = resp.bytes().await.unwrap().to_vec();
    (status, wire, binding)
}

/// What a sealed reply stream said, once opened to its end.
struct Reply {
    content_type: String,
    data: Vec<u8>,
    /// keepalives seen: empty data chunks the enclave sealed while the
    /// pipeline ran.
    keepalives: usize,
    /// the refusal the `Final` carried, if the stream was withdrawn.
    refused: Option<String>,
}

fn open_reply(keys: &airlock::handshake::SessionKeys, binding: &[u8], wire: &[u8]) -> Reply {
    let mut opener = bodyseal::StreamOpener::new(keys, binding);
    let items = opener.feed(wire).unwrap();
    assert!(
        opener.finished(),
        "the sealed stream must end with the Final marker"
    );
    let mut reply = Reply {
        content_type: String::new(),
        data: Vec::new(),
        keepalives: 0,
        refused: None,
    };
    for item in items {
        match item {
            OpenedItem::Head(ct) => reply.content_type = ct,
            OpenedItem::Data(bytes) if bytes.is_empty() => reply.keepalives += 1,
            OpenedItem::Data(bytes) => reply.data.extend(bytes),
            OpenedItem::Final => {}
            OpenedItem::Refused(reason) => reply.refused = Some(reason),
        }
    }
    reply
}

/// The pipeline's refusal, after the head: a `200` whose sealed stream ends
/// in a `Final` carrying the token.
async fn refused_in_band(
    gateway_url: &str,
    token: &str,
    keys: &airlock::handshake::SessionKeys,
    archive: &[u8],
) -> String {
    let (status, wire, binding) = post_sealed(gateway_url, token, keys, archive).await;
    assert_eq!(status, reqwest::StatusCode::OK, "the head is committed before the pipeline runs");
    let reply = open_reply(keys, &binding, &wire);
    assert_eq!(reply.content_type, "application/zstd");
    assert!(reply.data.is_empty(), "a refused stream carries no archive");
    reply.refused.expect("a refusal rides the Final marker")
}

fn rcodesign_verify(rcodesign: &Path, macho: &Path) {
    let out = Command::new(rcodesign)
        .arg("verify")
        .arg(macho)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rcodesign verify {}: {}{}",
        macho.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn an_unsigned_bundle_comes_back_signed_and_verifies() {
    let work = tempfile::tempdir().unwrap();
    let enclave = enclave();
    let gateway_url = boot_gateway(&enclave, Some(tools(work.path(), StubNotary::Accepts))).await;
    let gw = Gateway::local(gateway_url.clone());
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    upload_identity(&gw, &seal_pk, "release-sign").await;

    let stage = tempfile::tempdir().unwrap();
    let bundle = fixture::stage_bundle(stage.path(), sign::BUNDLE_ID);
    let archive = fixture::archive(&bundle);

    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();
    let (status, wire, binding) = post_sealed(&gateway_url, &token, &keys, &archive).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&wire)
    );
    let reply = open_reply(&keys, &binding, &wire);
    assert_eq!(reply.content_type, "application/zstd");
    assert!(reply.refused.is_none());
    let signed_archive = reply.data;
    assert!(
        !wire.windows(4).any(|w| w == b"\x28\xb5\x2f\xfd"),
        "the wire reply must not carry the zstd frame in the clear"
    );

    // Unpack the reply where the release lane would and verify what came back.
    let out = tempfile::tempdir().unwrap();
    let decoder = zstd::Decoder::new(&signed_archive[..]).unwrap();
    tar::Archive::new(decoder).unpack(out.path()).unwrap();
    let signed = out.path().join(sign::BUNDLE_NAME);
    assert!(
        signed
            .join("Contents/_CodeSignature/CodeResources")
            .is_file()
    );
    assert!(
        std::fs::symlink_metadata(signed.join("Contents/MacOS/views"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the views link survives the round trip"
    );
    let rcodesign = fixture::rcodesign();
    for executable in ["ducktape-launcher", "ducktape-app"] {
        rcodesign_verify(&rcodesign, &signed.join("Contents/MacOS").join(executable));
    }
    assert_eq!(
        std::fs::read_dir(work.path()).unwrap().count(),
        0,
        "the per-request work directory is removed"
    );
}

#[tokio::test]
async fn refusals_are_named_tokens_and_the_work_dir_is_gone_after_each() {
    let work = tempfile::tempdir().unwrap();
    let enclave = enclave();
    let gateway_url = boot_gateway(&enclave, Some(tools(work.path(), StubNotary::Rejects))).await;
    let gw = Gateway::local(gateway_url.clone());
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    upload_identity(&gw, &seal_pk, "release-sign").await;
    gw.upload_sealed_credential(
        &seal_pk,
        "claude",
        CredentialKind::Claude,
        &CredentialPayload::Bearer {
            access_token: "tok".into(),
        },
    )
    .await
    .unwrap();

    // a model credential pointed at the signing route
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "claude", &WorkRef::Direct)
        .await
        .unwrap();
    let (status, wire, _) = post_sealed(&gateway_url, &token, &keys, b"anything").await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert_eq!(wire, b"credential_kind_mismatch");

    // a plaintext session on a signing credential
    let token = gw
        .open_session(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}{ROUTE}"))
        .bearer_auth(&token)
        .body(b"anything".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.text().await.unwrap(),
        "airlock: signing requires a sealed session"
    );

    // the wrong shape, then a notary rejection, on one sealed session
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();
    let stage = tempfile::tempdir().unwrap();
    let other = fixture::stage_bundle(stage.path(), "dev.ducktape.other");
    let reason = refused_in_band(&gateway_url, &token, &keys, &fixture::archive(&other)).await;
    assert_eq!(reason, "bundle_shape_refused");

    let stage = tempfile::tempdir().unwrap();
    let bundle = fixture::stage_bundle(stage.path(), sign::BUNDLE_ID);
    let reason = refused_in_band(&gateway_url, &token, &keys, &fixture::archive(&bundle)).await;
    assert_eq!(reason, "notary_rejected");
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_gateway_without_a_toolchain_mounts_no_signing_route() {
    let enclave = enclave();
    let gateway_url = boot_gateway(&enclave, None).await;
    let gw = Gateway::local(gateway_url.clone());
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    upload_identity(&gw, &seal_pk, "release-sign").await;
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();
    let (status, _, _) = post_sealed(&gateway_url, &token, &keys, b"anything").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_missing_tool_is_refused_by_name() {
    let work = tempfile::tempdir().unwrap();
    let enclave = enclave();
    let mut tools = tools(work.path(), StubNotary::Accepts);
    tools.rcodesign = PathBuf::from("/nonexistent/rcodesign");
    let gateway_url = boot_gateway(&enclave, Some(tools)).await;
    let gw = Gateway::local(gateway_url.clone());
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    upload_identity(&gw, &seal_pk, "release-sign").await;
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();
    let stage = tempfile::tempdir().unwrap();
    let bundle = fixture::stage_bundle(stage.path(), sign::BUNDLE_ID);
    let reason = refused_in_band(&gateway_url, &token, &keys, &fixture::archive(&bundle)).await;
    assert_eq!(reason, "tool_missing");
}

/// The head is committed before the pipeline runs and the stream stays live
/// while it does: a notary that answers after two keepalive intervals is
/// preceded by keepalives on the wire, and the archive still arrives whole.
/// The only wait here is the stub's own delay.
#[tokio::test]
async fn the_head_is_committed_before_the_pipeline_and_keepalives_span_the_wait() {
    let work = tempfile::tempdir().unwrap();
    let enclave = enclave();
    let delay = sign::KEEPALIVE_INTERVAL * 2 + Duration::from_millis(500);
    let gateway_url = boot_gateway(
        &enclave,
        Some(tools(work.path(), StubNotary::AcceptsAfter(delay))),
    )
    .await;
    let gw = Gateway::local(gateway_url.clone());
    let seal_pk = attested_seal_pk(&gw, &enclave).await;
    upload_identity(&gw, &seal_pk, "release-sign").await;
    let stage = tempfile::tempdir().unwrap();
    let bundle = fixture::stage_bundle(stage.path(), sign::BUNDLE_ID);
    let archive = fixture::archive(&bundle);
    let (token, keys) = gw
        .open_session_sealed(&seal_pk, "release-sign", &WorkRef::Direct)
        .await
        .unwrap();

    let aad = bodyseal::request_aad("POST", ROUTE);
    let sealed = bodyseal::seal_request(&keys, &aad, &archive);
    let binding = bodyseal::request_binding(&sealed);
    let resp = reqwest::Client::new()
        .post(format!("{gateway_url}{ROUTE}"))
        .bearer_auth(&token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .body(sealed)
        .send()
        .await
        .unwrap();
    // the head, well before the notary answered
    let head_at = std::time::Instant::now();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let mut opener = bodyseal::StreamOpener::new(&keys, &binding);
    let mut resp = resp;
    let first = resp.chunk().await.unwrap().expect("the sealed head chunk");
    let items = opener.feed(&first).unwrap();
    assert!(
        matches!(items.first(), Some(OpenedItem::Head(ct)) if ct == "application/zstd"),
        "{items:?}"
    );
    assert!(
        head_at.elapsed() < delay,
        "the head must not wait for the notary"
    );
    let wire = resp.bytes().await.unwrap();
    let reply = open_reply(&keys, &binding, &[first.to_vec(), wire.to_vec()].concat());
    assert!(reply.keepalives >= 1, "keepalives: {}", reply.keepalives);
    assert!(reply.refused.is_none());
    assert!(!reply.data.is_empty(), "the archive follows the wait");
}
