//! Airlock over the gateway overlay: `cred != compute` end to end on two real,
//! TUN-less WireGuard nodes.
//!
//! Alice (the credential node) runs an airlock gateway on loopback, seals a
//! credential into it, registers its port, and publishes `airlock.alice.duck`
//! as a `LoopbackHttp` route with `allow_authorization = true`. The airlock
//! CLIENT then drives the full protocol — attest, session-key handshake, and a
//! proxied `/v1/messages` — through BOB's browser-gateway door and over the
//! authenticated WireGuard overlay to Alice's enclave, and gets the swapped
//! credential's reply back. This proves the remote topology, minus
//! silicon-backed quote GENERATION and the real Anthropic
//! upstream (a testkit-minted SNP quote — checked by the REAL chain verifier
//! under the test enclave's roots — and a mock upstream stand in, so the test
//! is deterministic and spends no quota).
//!
//! The session-key handshake is load-bearing HERE: the quote is fetched and
//! verified OVER the untrusted overlay before any token is derived, so a relaying
//! node cannot substitute its key or read the token.
//!
//! Real quote verification is the opt-in `verify` feature (off by default);
//! this whole file compiles out without it. Run with
//! `cargo test -p node-bin --test airlock_gateway_e2e --features verify`.
#![cfg(feature = "verify")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Cluster, create_account, submit_frame};
use commonware_cryptography::{Signer as _, ed25519};
use gateway::{
    CredentialRecord, DuckDnsName, GatewayMsg, GatewayQuery, GatewayReply, MemberAuthorization,
    RouteAudience, RouteDefinition, RouteMethod, RouteName, RoutePolicy, RouteStatement,
    RouteTarget, SetCredentialStatement,
};

use airlock::attest::{self, Measurement};
use airlock::client::Gateway as AirlockClient;
use airlock::server::{self, AttestMode, GatewayConfig};
use airlock::wire::{CredentialKind, CredentialPayload, WorkRef};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::runtime::Runtime;

const READY: Duration = Duration::from_secs(180);
const FINALIZE: Duration = Duration::from_secs(60);

fn measurement_hex() -> String {
    "11".repeat(attest::MRTD_LEN)
}

/// ONE test enclave (measures `0x11`x48) shared by the tests in this file. Its
/// minted SNP chain is verified through the REAL `airlock::verify` path — but
/// only under its own roots, never under the AMD builtins.
fn test_enclave() -> &'static Arc<airlock::testkit::SnpTestEnclave> {
    static ENCLAVE: std::sync::OnceLock<Arc<airlock::testkit::SnpTestEnclave>> =
        std::sync::OnceLock::new();
    ENCLAVE.get_or_init(|| {
        let m = Measurement::from_hex(&measurement_hex()).unwrap();
        Arc::new(airlock::testkit::SnpTestEnclave::new(&m).unwrap())
    })
}

// ----- duckdns + route helpers (mirrors gateway_e2e) ------------------------

fn resolve_handle(cluster: &Cluster, reader: usize, handle: &str) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Resolve {
            name: DuckDnsName {
                handle: handle.into(),
            },
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Resolved(Some(account)) => Some(account.account_id),
        _ => None,
    }
}

/// The airlock route: `LoopbackHttp` with `allow_authorization = true` (the
/// session-token bearer must reach the enclave) and a real 4 MiB response cap.
/// GET (attestation) + POST (session, messages) are the methods the airlock
/// protocol uses.
///
/// NB: `max_response_bytes = 0` ("unbounded/SSE") is enforced LITERALLY as a
/// 0-byte cap on today's BUFFERED proxy path (`proxy_current`), so it 502s any
/// non-empty response. Unbounded/streaming is the deferred SSE-over-overlay
/// slice; until then a real cap is required, so short turns fit and long
/// interactive streaming waits on that slice.
fn signed_airlock_route(
    member: &ed25519::PrivateKey,
    chain: &str,
    account: u64,
    publisher: &[u8],
    revision: u64,
) -> GatewayMsg {
    signed_loopback_route(
        member,
        chain,
        account,
        publisher,
        "airlock",
        revision,
        4 * 1024 * 1024,
        true,
    )
}

/// A member-signed LoopbackHttp route. `max_response_bytes == 0` = unbounded
/// streaming (SSE).
#[allow(clippy::too_many_arguments)]
fn signed_loopback_route(
    member: &ed25519::PrivateKey,
    chain: &str,
    account: u64,
    publisher: &[u8],
    name: &str,
    revision: u64,
    max_response_bytes: u64,
    allow_authorization: bool,
) -> GatewayMsg {
    let statement = RouteStatement {
        chain_id: chain.into(),
        account_id: account,
        name: RouteName::named(name),
        publisher_node: publisher.to_vec(),
        revision,
        route: Some(RouteDefinition {
            target: RouteTarget::LoopbackHttp,
            policy: RoutePolicy {
                audience: RouteAudience::Network,
                methods: vec![RouteMethod::Get, RouteMethod::Head, RouteMethod::Post],
                max_request_bytes: 1024 * 1024,
                max_response_bytes,
                allow_authorization,
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

fn airlock_route_revision(cluster: &Cluster, reader: usize, account: u64) -> Option<u64> {
    route_revision(cluster, reader, account, "airlock")
}

fn route_revision(cluster: &Cluster, reader: usize, account: u64, name: &str) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Get {
            account_id: account,
            name: RouteName::named(name),
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

// ----- in-process mock Anthropic upstream + airlock gateway -----------------

/// A mock Anthropic upstream: `/oauth/token` mints `acc-N`; `/v1/messages`
/// accepts ONLY `Bearer acc-N` (so a 200 proves the gateway swapped the session
/// token for the real credential) and returns the `AIRLOCK-OK` marker.
#[derive(Default)]
struct MockUpstream {
    n: Mutex<u64>,
}

async fn mock_oauth(State(st): State<Arc<MockUpstream>>) -> Json<serde_json::Value> {
    let mut n = st.n.lock().unwrap();
    *n += 1;
    Json(
        json!({ "access_token": format!("acc-{n}"), "refresh_token": format!("ref-{n}"), "expires_in": 3600 }),
    )
}

async fn mock_messages(
    State(st): State<Arc<MockUpstream>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let want = format!("Bearer acc-{}", *st.n.lock().unwrap());
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != want {
        return (axum::http::StatusCode::UNAUTHORIZED, "bad upstream bearer").into_response();
    }
    (
        [("content-type", "text/event-stream")],
        "data: AIRLOCK-OK\n\n",
    )
        .into_response()
}

async fn bind_and_serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Boot the mock upstream and the airlock gateway (testkit quoter, verified by
/// the real SNP verifier) pointed at it.
/// Returns `(gateway_base_url, gateway_loopback_port)`.
async fn boot_gateway_and_upstream() -> (String, u16) {
    let upstream = bind_and_serve(
        Router::new()
            .route("/oauth/token", post(mock_oauth))
            .route("/v1/messages", post(mock_messages))
            .with_state(Arc::new(MockUpstream::default())),
    )
    .await;

    let (app, vendor) = server::build_with_quoter(
        GatewayConfig {
            // build_with_quoter takes the vendor as an explicit arg; the config's
            // attest field is unused on this path (Tsm carries the TEE vendor).
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
        test_enclave().quoter(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(vendor, "snp");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), port)
}

/// An attested gateway carrying the SIGNING toolchain (`POST /sign/macos-bundle`
/// mounted), with Apple stubbed: `rcodesign sign` is real, the notary answers
/// `Accepts`. The shape `bin/airlock-gateway` serves, minus Apple.
async fn boot_signing_gateway(
    work_root: &std::path::Path,
    notary: airlock::sign::StubNotary,
) -> (String, u16) {
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
            sign: Some(airlock::sign::Tools {
                rcodesign: airlock::sign::fixture::rcodesign(),
                entitlements: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../app/packaging/entitlements.plist"),
                work_root: work_root.to_path_buf(),
                notary: airlock::sign::Notary::Stub(notary),
            }),
        },
        "snp",
        test_enclave().quoter(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(vendor, "snp");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), port)
}

/// Verify the gateway's quote against the test enclave's roots and read the
/// seal key out of the attested REPORTDATA — the trust step the operator's
/// `cred seal` performs.
async fn attested_seal_pk(gw: &AirlockClient) -> [u8; 32] {
    let (quote, _vendor) = gw.fetch_quote().await.unwrap();
    let expected = Measurement::from_hex(&measurement_hex()).unwrap();
    let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
        .await
        .unwrap();
    attest::split_report_data(&rd).0
}

/// The owner-signed on-chain record of a credential the gateway holds: what
/// `ducktape release sign-bundle --credential <name>` resolves the enclave
/// from (owner → handle → `airlock.<handle>.duck`; `seal_pk` pinned).
fn signed_set_credential(
    owner: &ed25519::PrivateKey,
    chain: &str,
    owner_account: u64,
    publisher_node: &[u8],
    name: &str,
    kind: gateway::CredentialKind,
    seal_pk: [u8; 32],
) -> GatewayMsg {
    let statement = SetCredentialStatement {
        chain_id: chain.into(),
        record: CredentialRecord {
            name: name.into(),
            owner_account,
            publisher_node: publisher_node.to_vec(),
            kind,
            seal_pk,
            grants: std::collections::BTreeSet::new(),
        },
    };
    let signature = owner
        .sign(
            gateway::GATEWAY_CREDENTIAL_NS,
            &gateway::set_credential_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::SetCredential {
        statement,
        authorization: MemberAuthorization {
            signer: owner.public_key().as_ref().to_vec(),
            signature,
        },
    }
}

fn credential_registered(cluster: &Cluster, reader: usize, name: &str) -> Option<()> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Credential {
            name: name.to_string(),
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Credential(record) => record.map(|_| ()),
        _ => None,
    }
}

/// One validator node with alice's account and handle, and the enclave's
/// port bound under BOTH airlock labels (`airlock`, the model lane, and
/// `airlock-sign`, the signing lane — one gateway, two signed caps). Returns
/// alice's account and node identity; the routes are the caller's to publish.
fn alice_fronts_the_gateway(
    cluster: &mut Cluster,
    gw_port: u16,
    alice: &ed25519::PrivateKey,
) -> (u64, Vec<u8>) {
    // No peer, so no handshake ever completes — but the gateway plane still
    // binds and serves the node's own routes locally.
    cluster.wireguard = true;
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", READY);
    cluster.wait_marker(0, "converged root_hash=", READY);
    cluster.wait_marker(0, "gateway plane: overlay stream bound", READY);

    let alice_node = Cluster::identity(0);
    let alice_account = create_account(cluster, 0, alice, "alice");

    submit_frame(
        cluster,
        0,
        alice,
        "gateway",
        &gateway::encode_msg(&GatewayMsg::SetHandle {
            handle: Some("alice".into()),
        }),
    );
    cluster.await_committed(0, "alice.duck resolution", FINALIZE, || {
        resolve_handle(cluster, 0, "alice")
    });

    let workspace = cluster.workspace(0);
    for label in ["airlock", "airlock-sign"] {
        let (ok, output) = cluster.run_verb(&[
            "gateway",
            "bind",
            "--workspace",
            workspace.to_str().unwrap(),
            "--label",
            label,
            "--port",
            &gw_port.to_string(),
            // whose signed route this node serves under the label — stated, so
            // the harness inherits no active wallet from the host keystore.
            "--account",
            &alice_account.to_string(),
        ]);
        assert!(ok, "airlock gateway port bind ({label}) failed: {output}");
    }
    (alice_account, alice_node)
}

/// One validator node serving a loopback gateway under `airlock.alice.duck`:
/// alice's account and handle, the port bound under the `airlock` label, the
/// signed route committed. Returns alice's key, account and node identity.
fn alice_serves_airlock(
    cluster: &mut Cluster,
    gw_port: u16,
) -> (ed25519::PrivateKey, u64, Vec<u8>) {
    let alice = ed25519::PrivateKey::from_seed(42);
    let (alice_account, alice_node) = alice_fronts_the_gateway(cluster, gw_port, &alice);
    submit_frame(
        cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_airlock_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            1,
        )),
    );
    cluster.await_committed(0, "airlock route revision 1", FINALIZE, || {
        (airlock_route_revision(cluster, 0, alice_account) == Some(1)).then_some(())
    });
    (alice, alice_account, alice_node)
}

/// The published `max_request_bytes` of one of alice's routes.
fn route_request_cap(cluster: &Cluster, account: u64, name: &str) -> Option<u64> {
    let bytes = cluster.query(
        0,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Get {
            account_id: account,
            name: RouteName::named(name),
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Route(record) => Some((*record)?.statement.route?.policy.max_request_bytes),
        _ => None,
    }
}

/// Incompressible bytes: a cap or a size on an archive is measured after
/// zstd, which folds a constant fill to nothing.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    std::iter::repeat_with(|| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    })
    .take(len)
    .collect()
}

/// `ducktape release sign-bundle` through one real node, at release size and
/// on Apple's clock: the operator enrols the identity with `cred seal
/// --vendor apple-codesign` (the quote verified against roots given out of
/// band, the record and both routes committed by the verb — nothing
/// hand-signed), then the verb resolves the credential's on-chain record to
/// `airlock-sign.alice.duck`, opens a sealed session through the node's
/// browser gateway, and the enclave signs a ≥48 MB bundle with a throwaway
/// Developer-ID-shaped identity while the stubbed notary answers later than
/// the proxy's head deadline. What comes back unpacks to a bundle `rcodesign
/// verify` accepts, in place of the unsigned one — the `make release-app`
/// sequence under `DUCKTAPE_SIGN_VIA=airlock`, minus `archive.sh`'s
/// macOS-only checks.
#[test]
fn release_sign_bundle_round_trips_through_the_node() {
    use airlock::codesign::fixture::{self as cred, Marker};
    use airlock::sign::{self, StubNotary, fixture};

    let rt = Runtime::new().unwrap();
    let work = tempfile::tempdir().unwrap();
    // Apple answers after the proxy's head deadline: the head must already
    // be committed and the stream kept live for the reply to arrive at all.
    let notary_delay = noded::PROXY_REPLY_TIMEOUT + Duration::from_secs(5);
    let (gw_base, gw_port) = rt.block_on(boot_signing_gateway(
        work.path(),
        StubNotary::AcceptsAfter(notary_delay),
    ));

    // a model credential beside the signing identity, for the kind refusal
    // below; sealed in by the client and hand-registered, as a model
    // credential's record is not `cred seal`'s to write.
    let seal_pk = rt.block_on(async {
        let gw = AirlockClient::local(gw_base.clone());
        let seal_pk = attested_seal_pk(&gw).await;
        gw.upload_sealed_credential(
            &seal_pk,
            "compute-node",
            CredentialKind::Claude,
            &CredentialPayload::Bearer {
                access_token: "seed".into(),
            },
        )
        .await
        .unwrap();
        seal_pk
    });

    let mut cluster = Cluster::new(&[0], &[0]);
    // alice's wallet as the operator holds it: an encrypted key file the
    // verbs open with the password on stdin.
    let wallet = cluster.workspace(0).join("alice.key");
    std::fs::create_dir_all(cluster.workspace(0)).unwrap();
    let (_, alice) = keystore::userkey::mint_user_key(&wallet, "alice-fixture-password")
        .expect("mint alice's wallet");
    let (alice_account, alice_node) = alice_fronts_the_gateway(&mut cluster, gw_port, &alice);
    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_set_credential(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            "compute-node",
            gateway::CredentialKind::Claude,
            seal_pk,
        )),
    );
    cluster.await_committed(0, "credential compute-node registered", FINALIZE, || {
        credential_registered(&cluster, 0, "compute-node")
    });

    // Credential Provider: `cred seal --vendor apple-codesign` verifies the
    // quote against the test chain's roots (given out of band, as an
    // air-gapped operator gives AMD's), seals the identity in, and writes
    // the on-chain half: the record under the attested seal key and the
    // account's two airlock routes.
    let material = tempfile::tempdir().unwrap();
    let (ark, ask, vcek) = test_enclave().root_files().unwrap();
    let file = |name: &str, bytes: &[u8]| -> String {
        let path = material.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    };
    use base64::Engine as _;
    let p12 = base64::engine::general_purpose::STANDARD
        .decode(cred::p12_b64(cred::TEAM_ID, Marker::DeveloperIdApplication))
        .unwrap();
    let ark = file("ark.pem", &ark);
    let ask = file("ask.pem", &ask);
    let vcek = file("vcek.der", &vcek);
    let p12 = file("id.p12", &p12);
    let p12_password = file(
        "p12.password",
        format!("{}\n", cred::P12_PASSWORD).as_bytes(),
    );
    let api_key = file("AuthKey.json", cred::api_key_json().as_bytes());
    let node = cluster.http_base(0);
    let measurement = measurement_hex();
    let (ok, output) = cluster.run_verb_with(
        &[
            "user",
            "cred",
            "seal",
            "--host",
            &gw_base,
            "--attest",
            "snp",
            "--snp-product",
            "genoa",
            "--snp-ark",
            &ark,
            "--snp-ask",
            &ask,
            "--snp-vcek",
            &vcek,
            "--measurement",
            &measurement,
            "--vendor",
            "apple-codesign",
            "--name",
            "release-sign",
            "--p12",
            &p12,
            "--p12-password-file",
            &p12_password,
            "--api-key",
            &api_key,
            "--team-id",
            cred::TEAM_ID,
            "--key",
            wallet.to_str().unwrap(),
            "--node",
            &node,
        ],
        &[],
        "alice-fixture-password\n",
    );
    assert!(ok, "cred seal: {output}\n{}", cluster.all_log_tails(40));
    assert!(output.contains("quote verified"), "{output}");
    assert!(output.contains("registered release-sign"), "{output}");
    assert!(output.contains("published airlock route"), "{output}");
    assert!(output.contains("published airlock-sign route"), "{output}");
    cluster.await_committed(0, "credential release-sign registered", FINALIZE, || {
        credential_registered(&cluster, 0, "release-sign")
    });
    cluster.await_committed(0, "both airlock routes published", FINALIZE, || {
        let model = route_request_cap(&cluster, alice_account, "airlock")?;
        let sign = route_request_cap(&cluster, alice_account, "airlock-sign")?;
        (model == 16 * 1024 * 1024 && sign == 256 * 1024 * 1024).then_some(())
    });

    // a wrong measurement never releases the identity: refused by name
    // before anything is uploaded or registered
    let (ok, output) = cluster.run_verb_with(
        &[
            "user",
            "cred",
            "seal",
            "--host",
            &gw_base,
            "--attest",
            "snp",
            "--snp-product",
            "genoa",
            "--snp-ark",
            &ark,
            "--snp-ask",
            &ask,
            "--snp-vcek",
            &vcek,
            "--measurement",
            &"22".repeat(attest::MRTD_LEN),
            "--vendor",
            "apple-codesign",
            "--name",
            "release-sign-2",
            "--p12",
            &p12,
            "--p12-password-file",
            &p12_password,
            "--api-key",
            &api_key,
            "--team-id",
            cred::TEAM_ID,
            "--key",
            wallet.to_str().unwrap(),
            "--node",
            &node,
        ],
        &[],
        "alice-fixture-password\n",
    );
    assert!(!ok, "{output}");
    assert!(output.contains("attestation_unverified"), "{output}");
    assert!(!output.contains("registered release-sign-2"), "{output}");

    // A release-size bundle: the fixture's shape carrying 48 MB of
    // incompressible views, well past the model lane's 16 MiB.
    let stage = tempfile::tempdir().unwrap();
    let unsigned = fixture::stage_bundle(stage.path(), sign::BUNDLE_ID);
    std::fs::write(
        unsigned.join("Contents/Resources/views/huge.wasm"),
        incompressible(48 * 1024 * 1024),
    )
    .unwrap();
    let unsigned_app = std::fs::read(unsigned.join("Contents/MacOS/ducktape-app")).unwrap();
    let out = stage.path().join("Ducktape-signed.tar.zst");
    let bundle_arg = unsigned.to_str().unwrap();
    let out_arg = out.to_str().unwrap();
    let stage_arg = stage.path().to_str().unwrap();

    // the wrong kind of credential is refused by name before a session opens
    let (ok, output) = cluster.run_verb(&[
        "release",
        "sign-bundle",
        bundle_arg,
        "--credential",
        "compute-node",
        "--out",
        out_arg,
        "--node",
        &node,
    ]);
    assert!(!ok, "{output}");
    assert!(output.contains("credential_kind_mismatch"), "{output}");
    assert!(!out.exists());

    // an unknown name says what is registered
    let (ok, output) = cluster.run_verb(&[
        "release",
        "sign-bundle",
        bundle_arg,
        "--credential",
        "nope",
        "--out",
        out_arg,
        "--node",
        &node,
    ]);
    assert!(!ok, "{output}");
    assert!(output.contains("unknown credential: nope"), "{output}");
    assert!(output.contains("release-sign"), "{output}");

    // the round trip, unpacked in place of the unsigned bundle
    let started = std::time::Instant::now();
    let (ok, output) = cluster.run_verb(&[
        "release",
        "sign-bundle",
        bundle_arg,
        "--credential",
        "release-sign",
        "--out",
        out_arg,
        "--unpack-into",
        stage_arg,
        "--node",
        &node,
    ]);
    assert!(ok, "sign-bundle: {output}\n{}", cluster.all_log_tails(40));
    assert!(
        started.elapsed() >= notary_delay,
        "the reply cannot precede the notary's answer"
    );
    assert!(output.contains("airlock-sign.alice.duck"), "{output}");
    assert!(output.contains("release_sign_submitted"), "{output}");
    assert!(output.contains("release_sign_received"), "{output}");
    let signed_archive = std::fs::read(&out).unwrap();
    assert!(
        signed_archive.len() >= 48 * 1024 * 1024,
        "the signed archive carries the views back ({} bytes)",
        signed_archive.len()
    );

    let signed = stage.path().join(sign::BUNDLE_NAME);
    sign::validate_layout(&signed).unwrap();
    let signed_app = std::fs::read(signed.join("Contents/MacOS/ducktape-app")).unwrap();
    assert_ne!(signed_app, unsigned_app, "the executable came back signed");
    assert_eq!(
        std::fs::metadata(signed.join("Contents/Resources/views/huge.wasm"))
            .unwrap()
            .len(),
        48 * 1024 * 1024
    );
    for executable in ["ducktape-launcher", "ducktape-app"] {
        let verify = std::process::Command::new(fixture::rcodesign())
            .arg("verify")
            .arg(signed.join("Contents/MacOS").join(executable))
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "rcodesign verify {executable}: {}{}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr)
        );
    }
    assert_eq!(
        std::fs::read_link(signed.join("Contents/MacOS/views")).unwrap(),
        std::path::PathBuf::from("../Resources/views"),
        "the views link survives the round trip"
    );
    assert!(
        std::fs::read_dir(work.path()).unwrap().next().is_none(),
        "the enclave's work dir is empty after the request"
    );
}

// TODO(full-spec, needs hardware): this proves the node-to-node overlay hop with
// a minted SNP chain (real verifier, test roots) and a mock upstream. The full
// 2-node + TEE run — silicon-backed quote GENERATION on a confidential VM and
// the real Anthropic API via the static bearer (PR #681) — is deferred to real
// hardware. See the design spec "TODO — full 2-node + TEE validation".
#[test]
fn airlock_over_gateway_two_wireguard_nodes() {
    let rt = Runtime::new().unwrap();

    // Alice's loopback services: the airlock gateway + the mock upstream it swaps
    // into. Both live in THIS process; the alice node subprocess reaches the
    // gateway over host loopback (127.0.0.1:port), exactly like a real deployment.
    let (gw_base, gw_port) = rt.block_on(boot_gateway_and_upstream());

    // Credential Provider (local, direct to the enclave loopback): verify the
    // quote, then seal a refresh token (the mock upstream mints access tokens).
    rt.block_on(async {
        let gw = AirlockClient::local(gw_base.clone());
        let (quote, _vendor) = gw.fetch_quote().await.unwrap();
        let expected = Measurement::from_hex(&measurement_hex()).unwrap();
        let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
            .await
            .unwrap();
        let seal_pk = attest::split_report_data(&rd).0;
        gw.upload_sealed_credential(
            &seal_pk,
            "compute-node",
            CredentialKind::Claude,
            &CredentialPayload::Refresh {
                refresh_token: "seed".into(),
                access_token: String::new(),
                expires_at: 0,
            },
        )
        .await
        .unwrap();
    });

    // Two real WireGuard nodes: alice (0, publisher) and bob (1, compute).
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

    // Alice founds her account through her node (a user-signed Create); Bob's
    // node is only ever a caller, and a node is never an account.
    let alice = ed25519::PrivateKey::from_seed(42);
    let alice_node = Cluster::identity(0);
    let alice_account = create_account(&cluster, 0, &alice, "alice");

    // Alice maps `alice.duck`, so `airlock.alice.duck` resolves to her account.
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
            resolve_handle(&cluster, reader, "alice")
        });
        assert_eq!(resolved, alice_account);
    }

    // Register the gateway's loopback port node-locally, then publish the signed
    // LoopbackHttp route on consensus.
    let workspace = cluster.workspace(0);
    let (ok, output) = cluster.run_verb(&[
        "gateway",
        "bind",
        "--workspace",
        workspace.to_str().unwrap(),
        "--label",
        "airlock",
        "--port",
        &gw_port.to_string(),
        // whose signed route this node serves under the label — stated, so
        // the harness inherits no active wallet from the host keystore.
        "--account",
        &alice_account.to_string(),
    ]);
    assert!(ok, "airlock gateway port bind failed: {output}");

    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_airlock_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            1,
        )),
    );
    cluster.await_committed(1, "airlock route revision 1", FINALIZE, || {
        (airlock_route_revision(&cluster, 1, alice_account) == Some(1)).then_some(())
    });

    // Bob's browser-gateway origin base — the `via` a compute-side host process
    // posts to (no Origin header ⇒ it passes the only guard).
    let (status, browser) = cluster.http(1, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let via = browser["base"].as_str().unwrap().to_string();

    // THE PROOF: drive the whole airlock protocol from Bob, over the overlay, to
    // Alice's enclave. Every hop is REMOTE (bob -> overlay -> alice -> loopback).
    let reply = rt.block_on(async {
        let gw = AirlockClient::remote("airlock.alice.duck".into(), via);
        let expected = Measurement::from_hex(&measurement_hex()).unwrap();

        // 1) attest OVER the overlay, verify the quote, read the attested seal_pk.
        let (quote, _vendor) = gw.fetch_quote().await.expect("fetch quote over overlay");
        let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
            .await
            .unwrap();
        let seal_pk = attest::split_report_data(&rd).0;

        // 2) session-key handshake OVER the overlay → scoped token.
        let token = gw
            .open_session(&seal_pk, "compute-node", &WorkRef::Direct)
            .await
            .expect("handshake over overlay");

        // 3) proxied /v1/messages with the session-token bearer (forwarded because
        //    the route set allow_authorization). The gateway swaps it for the real
        //    credential and the mock upstream replies AIRLOCK-OK.
        let resp = gw
            .route(gw.http().post(gw.url("/v1/messages")))
            .bearer_auth(&token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .expect("proxied messages over overlay");
        let status = resp.status();
        let body = resp.text().await.unwrap();
        (status, body)
    });

    assert_eq!(
        reply.0,
        reqwest::StatusCode::OK,
        "overlay proxied call: {reply:?}"
    );
    assert!(
        reply.1.contains("AIRLOCK-OK"),
        "the swapped credential's reply must return over the overlay: {reply:?}"
    );
}

/// Single-node self-serve: ONE node publishes `airlock.alice.duck` and reaches
/// its OWN route through its browser-gateway (publisher == self → `serve_current`,
/// no WireGuard peer needed). This exercises the whole airlock-over-gateway path
/// — route publish, `x-duck-authority` resolution, the browser door, the
/// `allow_authorization` bearer forward, `proxy_loopback`, and the airlock
/// attest+handshake+swap — MINUS the node-to-node overlay hop (which
/// `airlock_over_gateway_two_wireguard_nodes` covers, on a box where inline
/// 2-node WireGuard peers reliably). Runs green where the 2-node harness can't.
#[test]
fn airlock_single_node_self_serves_its_own_route() {
    let rt = Runtime::new().unwrap();

    let (gw_base, gw_port) = rt.block_on(boot_gateway_and_upstream());
    rt.block_on(async {
        let gw = AirlockClient::local(gw_base.clone());
        let (quote, _vendor) = gw.fetch_quote().await.unwrap();
        let expected = Measurement::from_hex(&measurement_hex()).unwrap();
        let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
            .await
            .unwrap();
        let seal_pk = attest::split_report_data(&rd).0;
        gw.upload_sealed_credential(
            &seal_pk,
            "self",
            CredentialKind::Claude,
            &CredentialPayload::Refresh {
                refresh_token: "seed".into(),
                access_token: String::new(),
                expires_at: 0,
            },
        )
        .await
        .unwrap();
    });

    let mut cluster = Cluster::new(&[0], &[0]);
    alice_serves_airlock(&mut cluster, gw_port);

    let (status, browser) = cluster.http(0, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let via = browser["base"].as_str().unwrap().to_string();

    let reply = rt.block_on(async {
        let gw = AirlockClient::remote("airlock.alice.duck".into(), via);
        let expected = Measurement::from_hex(&measurement_hex()).unwrap();
        let (quote, _vendor) = gw.fetch_quote().await.expect("fetch quote through the gateway");
        let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
            .await
            .unwrap();
        let seal_pk = attest::split_report_data(&rd).0;
        // SEALED session: the request/response bodies cross the overlay as
        // ciphertext (streaming + body AEAD combined, over the real wire).
        let (token, keys) = gw
            .open_session_sealed(&seal_pk, "self", &WorkRef::Direct)
            .await
            .expect("sealed handshake through the gateway");
        let plaintext =
            br#"{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
        let aad = airlock::bodyseal::request_aad("POST", "/v1/messages");
        let sealed_body = airlock::bodyseal::seal_request(&keys, &aad, plaintext);
        let resp = gw
            .route(gw.http().post(gw.url("/v1/messages")))
            .bearer_auth(&token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("content-type", "application/json")
            .header(airlock::bodyseal::SEAL_HEADER, airlock::bodyseal::SEAL_V1)
            .body(sealed_body.clone())
            .send()
            .await
            .expect("proxied messages through the gateway");
        let status = resp.status();
        let wire = resp.bytes().await.unwrap();
        assert!(
            !wire.windows(10).any(|w| w == b"AIRLOCK-OK"),
            "the overlay must carry ciphertext, never the plaintext reply"
        );
        let mut opener = airlock::bodyseal::StreamOpener::new(&keys, &airlock::bodyseal::request_binding(&sealed_body));
        let items = opener.feed(&wire).expect("unseal the proxied reply");
        assert!(opener.finished(), "sealed reply must end with the Final marker");
        let body: Vec<u8> = items
            .into_iter()
            .filter_map(|item| match item {
                airlock::bodyseal::OpenedItem::Data(data) => Some(data),
                _ => None,
            })
            .flatten()
            .collect();
        (status, String::from_utf8_lossy(&body).into_owned())
    });

    assert_eq!(
        reply.0,
        reqwest::StatusCode::OK,
        "self-served proxied call: {reply:?}"
    );
    assert!(
        reply.1.contains("AIRLOCK-OK"),
        "the swapped credential's reply must return: {reply:?}"
    );
}

/// Streamed SSE through the gateway frame wire on one real node, plus the
/// running response cap. The upstream sends ONE chunk and then BLOCKS until
/// the client has read it — a buffered proxy cannot pass this (it would
/// deadlock waiting for the body to end), so success proves end-to-end
/// streaming through browser door -> duplex frame wire -> loopback upstream.
#[test]
fn gateway_streams_and_caps_over_the_frame_wire() {
    let rt = Runtime::new().unwrap();
    const CHUNK: usize = 64 * 1024;
    const TOTAL: usize = 6 * 1024 * 1024; // > the old 4 MiB buffered ceiling

    // Upstream: /events streams TOTAL bytes but holds after the first chunk
    // until the client releases the gate; /flood streams 1 MiB ungated.
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let upstream = {
        let gate = gate.clone();
        rt.block_on(async move {
            use axum::routing::get;
            let events = move || {
                let gate = gate.clone();
                async move {
                    let (tx, rx) =
                        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
                    tokio::spawn(async move {
                        let chunk = bytes::Bytes::from(vec![b'a'; CHUNK]);
                        if tx.send(Ok(chunk.clone())).await.is_err() {
                            return;
                        }
                        gate.notified().await; // the CLIENT read the first chunk
                        for _ in 0..(TOTAL / CHUNK - 1) {
                            if tx.send(Ok(chunk.clone())).await.is_err() {
                                return;
                            }
                        }
                    });
                    (
                        [("content-type", "text/event-stream")],
                        axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
                            rx,
                        )),
                    )
                }
            };
            // Sized overflow: Content-Length is declared, so the proxy can
            // refuse BEFORE the head (502) instead of truncating.
            let flood = || async {
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from(vec![b'b'; 1024 * 1024]),
                )
            };
            // Unsized overflow: chunked, no Content-Length — the head commits,
            // then the RUNNING cap truncates the body mid-stream.
            let flood_chunked = || async {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
                tokio::spawn(async move {
                    let chunk = bytes::Bytes::from(vec![b'c'; CHUNK]);
                    for _ in 0..16 {
                        if tx.send(Ok(chunk.clone())).await.is_err() {
                            return;
                        }
                    }
                });
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
                )
            };
            bind_and_serve(
                Router::new()
                    .route("/events", get(events))
                    .route("/flood", get(flood))
                    .route("/flood-chunked", get(flood_chunked)),
            )
            .await
        })
    };
    let upstream_port: u16 = upstream.rsplit(':').next().unwrap().parse().unwrap();

    let mut cluster = Cluster::new(&[0], &[0]);
    cluster.wireguard = true;
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", READY);
    cluster.wait_marker(0, "converged root_hash=", READY);
    cluster.wait_marker(0, "gateway plane: overlay stream bound", READY);

    let alice = ed25519::PrivateKey::from_seed(42);
    let alice_node = Cluster::identity(0);
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
    cluster.await_committed(0, "alice.duck resolution", FINALIZE, || {
        resolve_handle(&cluster, 0, "alice")
    });

    let workspace = cluster.workspace(0);
    for label in ["sse", "capped"] {
        let (ok, output) = cluster.run_verb(&[
            "gateway",
            "bind",
            "--workspace",
            workspace.to_str().unwrap(),
            "--label",
            label,
            "--port",
            &upstream_port.to_string(),
            // whose signed route this node serves under the label — stated,
            // so the harness inherits no active wallet from the host keystore.
            "--account",
            &alice_account.to_string(),
        ]);
        assert!(ok, "{label} port bind failed: {output}");
    }
    // "sse": max_response_bytes 0 = unbounded stream; "capped": 64 KiB cap.
    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_loopback_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            "sse",
            1,
            0,
            false,
        )),
    );
    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&signed_loopback_route(
            &alice,
            &cluster.namespace,
            alice_account,
            &alice_node,
            "capped",
            1,
            CHUNK as u64,
            false,
        )),
    );
    cluster.await_committed(0, "both routes live", FINALIZE, || {
        (route_revision(&cluster, 0, alice_account, "sse") == Some(1)
            && route_revision(&cluster, 0, alice_account, "capped") == Some(1))
        .then_some(())
    });

    let (status, browser) = cluster.http(0, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let via = browser["base"].as_str().unwrap().to_string();

    // Unbounded stream: 6 MiB arrives; the first chunk is READ before the
    // upstream is allowed to send the rest.
    let total = rt.block_on({
        let via = via.clone();
        let gate = gate.clone();
        async move {
            use futures::StreamExt as _;
            let resp = reqwest::Client::new()
                .get(format!("{via}/events"))
                .header("x-duck-authority", "sse.alice.duck")
                .send()
                .await
                .expect("streamed GET through the gateway");
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            let mut stream = resp.bytes_stream();
            let first = stream
                .next()
                .await
                .expect("first chunk")
                .expect("first chunk ok");
            assert!(!first.is_empty());
            gate.notify_one(); // only now may the upstream finish
            let mut total = first.len();
            while let Some(chunk) = stream.next().await {
                total += chunk.expect("streamed chunk").len();
            }
            total
        }
    });
    assert_eq!(
        total, TOTAL,
        "the full 6 MiB must stream through the frame wire"
    );

    // Sized overflow (Content-Length declared): refused BEFORE the head.
    let sized_status = rt.block_on({
        let via = via.clone();
        async move {
            reqwest::Client::new()
                .get(format!("{via}/flood"))
                .header("x-duck-authority", "capped.alice.duck")
                .send()
                .await
                .expect("sized capped GET through the gateway")
                .status()
        }
    });
    assert_eq!(
        sized_status,
        reqwest::StatusCode::BAD_GATEWAY,
        "a declared over-cap length is refused pre-head"
    );

    // Unsized overflow: the head commits, then the RUNNING cap truncates.
    let (status, received, truncated) = rt.block_on(async move {
        use futures::StreamExt as _;
        let resp = reqwest::Client::new()
            .get(format!("{via}/flood-chunked"))
            .header("x-duck-authority", "capped.alice.duck")
            .send()
            .await
            .expect("capped GET through the gateway");
        let status = resp.status();
        let mut received = 0usize;
        let mut truncated = false;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => received += chunk.len(),
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        (status, received, truncated)
    });
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the head commits before the cap trips"
    );
    assert!(truncated, "an over-cap chunked body must fail closed");
    assert!(
        (1..=CHUNK).contains(&received),
        "the running cap must expose a prefix no larger than its signed byte boundary ({received})"
    );
}
