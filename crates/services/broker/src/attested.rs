//! Real TEE quote verification — the opt-in `verify` feature
//! (dcap-qvl/sev/a second reqwest-0.13+hickory DNS stack: see
//! `Cargo.toml`'s `verify = ["airlock/verify"]`). Reached only from
//! `open_airlock_session`'s `Attested` arm, through [`verify`] — resolved
//! there as `verify_attested`, alongside the by-name refusal an off build
//! uses instead. The everyday self-hosted `PinnedSealPk` path never calls
//! in here at all.

use airlock::attest::{self, AttestMode, Measurement};
use airlock::verify::{SnpProduct, SnpRoots, TdxRoots, TrustRoots, VcekSource};

use super::*;

/// Fetch + verify the gateway quote and return the attested seal key, via the
/// real vendor verifier (`airlock::verify`) against pinned Intel/AMD roots.
pub(crate) async fn verify(
    gateway: &Gateway,
    cfg: &AirlockConfig,
    measurement: &str,
    attest: &str,
) -> Result<[u8; 32], String> {
    let mode: AttestMode = attest
        .parse()
        .map_err(|e| format!("airlock attest mode: {e}"))?;
    let expected =
        Measurement::from_hex(measurement).map_err(|e| format!("airlock measurement: {e}"))?;
    let roots = trust_roots(cfg, mode)?;
    let (quote, _vendor) = gateway
        .fetch_quote()
        .await
        .map_err(|e| format!("airlock fetch quote: {e}"))?;
    let report_data = airlock::verify::verify_quote(&quote, &expected, &roots)
        .await
        .map_err(|e| format!("airlock verify: {e}"))?;
    Ok(attest::split_report_data(&report_data).0)
}

/// Production: pinned roots assembled from the config's raw SNP/TDX fields,
/// parsed HERE (the Intel root lives inside dcap-qvl, the AMD ARK/ASK inside
/// the sev builtins — nothing here can swap a trust anchor). Tests: an
/// injected override, compiled OUT of non-test builds, so an in-process test
/// enclave is verified through the real verify path.
fn trust_roots(cfg: &AirlockConfig, mode: AttestMode) -> Result<TrustRoots, String> {
    #[cfg(test)]
    if let Some(roots) = test_trust_roots().lock().unwrap().clone() {
        return Ok(roots);
    }
    match mode {
        AttestMode::Tdx => Ok(TrustRoots::Tdx(TdxRoots {
            pccs_url: cfg.pccs_url.clone(),
        })),
        AttestMode::Snp => {
            let product = cfg
                .snp_product
                .as_deref()
                .ok_or_else(|| {
                    "airlock attest=snp requires DUCKTAPE_AIRLOCK_SNP_PRODUCT (milan|genoa|turin)"
                        .to_string()
                })?
                .parse::<SnpProduct>()
                .map_err(|e| format!("airlock SNP product: {e}"))?;
            let vcek = cfg
                .snp_vcek
                .clone()
                .map(VcekSource::Der)
                .unwrap_or(VcekSource::Kds);
            SnpRoots::amd(product, vcek)
                .map(|r| TrustRoots::Snp(Box::new(r)))
                .map_err(|e| format!("airlock SNP roots: {e}"))
        }
    }
}

#[cfg(test)]
fn test_trust_roots() -> &'static std::sync::Mutex<Option<TrustRoots>> {
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<Option<TrustRoots>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Airlock credential source (execution/auth separation) --------------

    /// A mock Anthropic upstream the AIRLOCK GATEWAY swaps into: `/oauth/token`
    /// mints `acc-N`; `/v1/messages` accepts ONLY `Bearer acc-N` — so a 200
    /// proves the gateway swapped the session token for the real credential —
    /// and streams `AIRLOCK-OK`.
    async fn airlock_mock_anthropic() -> String {
        use axum::response::IntoResponse;
        let n = Arc::new(Mutex::new(0u64));
        let oauth_n = n.clone();
        let msg_n = n.clone();
        let app = Router::new()
            .route(
                "/oauth/token",
                post(move || {
                    let n = oauth_n.clone();
                    async move {
                        let mut n = n.lock().unwrap();
                        *n += 1;
                        axum::Json(json!({
                            "access_token": format!("acc-{n}"),
                            "refresh_token": format!("ref-{n}"),
                            "expires_in": 3600
                        }))
                    }
                }),
            )
            .route(
                "/v1/messages",
                post(move |headers: HeaderMap| {
                    let n = msg_n.clone();
                    async move {
                        let want = format!("Bearer acc-{}", *n.lock().unwrap());
                        let got = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if got != want {
                            return (
                                StatusCode::UNAUTHORIZED,
                                format!("want {want:?} got {got:?}"),
                            )
                                .into_response();
                        }
                        (
                            [("content-type", "text/event-stream")],
                            "event: content_block_delta\ndata: AIRLOCK-OK\n\n",
                        )
                            .into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// ONE test enclave (measures `0x11`×48) shared by every airlock test in
    /// this file, so the parallel tests all agree on the injected trust roots.
    /// Its minted chain verifies through the REAL SNP verifier — only under
    /// its own roots, never under the AMD builtins.
    fn test_enclave() -> &'static Arc<airlock::testkit::SnpTestEnclave> {
        static ENCLAVE: std::sync::OnceLock<Arc<airlock::testkit::SnpTestEnclave>> =
            std::sync::OnceLock::new();
        ENCLAVE.get_or_init(|| {
            let m = Measurement([0x11; attest::MRTD_LEN]);
            let enclave = Arc::new(airlock::testkit::SnpTestEnclave::new(&m).unwrap());
            // Route the verify path at the enclave's roots. Set once, same
            // value from every test — no cross-test races.
            *test_trust_roots().lock().unwrap() = Some(enclave.roots());
            enclave
        })
    }

    /// Boot an in-process airlock gateway (measures `0x11`×48) pointed at
    /// `upstream`, and return its base URL.
    async fn boot_airlock_gateway(upstream: &str) -> String {
        let (app, vendor) = airlock::server::build_with_quoter(
            airlock::server::GatewayConfig {
                attest: airlock::server::AttestMode::Tsm("snp".into()),
                seal_keypair: None,
                anthropic_base: upstream.into(),
                openai_base: upstream.into(),
                oauth_token_url: format!("{upstream}/oauth/token"),
                oauth_client_id: "test-client".into(),
                session_ttl_secs: 3600,
                clock: airlock::server::Clock::system(),
                sign: None,
            },
            "snp",
            test_enclave().quoter(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(vendor, "snp");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn airlock_broker_uses_the_gateway_as_credential_source() {
        let meas = "11".repeat(attest::MRTD_LEN);
        let upstream = airlock_mock_anthropic().await;
        let gateway_url = boot_airlock_gateway(&upstream).await;

        // Credential Provider: verify the gateway quote through the REAL SNP
        // verifier (under the test enclave's roots), then seal + upload the
        // refresh token (the broker never holds it — the gateway does, sealed).
        let gw = Gateway::local(gateway_url.clone());
        let (quote, _vendor) = gw.fetch_quote().await.unwrap();
        let expected = Measurement::from_hex(&meas).unwrap();
        let rd = airlock::verify::verify_quote(&quote, &expected, &test_enclave().roots())
            .await
            .unwrap();
        let seal_pk = attest::split_report_data(&rd).0;
        gw.upload_sealed_credential(
            &seal_pk,
            "test-sub",
            airlock::wire::CredentialKind::Claude,
            &airlock::wire::CredentialPayload::Refresh {
                refresh_token: "ref-seed".into(),
                access_token: String::new(),
                expires_at: 0,
            },
        )
        .await
        .unwrap();

        // Computation Provider: build the Anthropic broker in AIRLOCK mode —
        // NO host credential, just a verified gateway + session token.
        let (auth, messages_url) = AnthropicAuth::airlock(AirlockConfig {
            kind: airlock::wire::CredentialKind::Claude,
            gateway: AirlockGateway::Local { url: gateway_url },
            trust: AirlockTrust::Attested {
                measurement: meas,
                attest: "snp".into(),
            },
            sub: "test-sub".into(),
            work: WorkRef::Direct,
            snp_product: None, // the test roots override supplies the chain
            snp_vcek: None,
            pccs_url: None,
        })
        .await
        .unwrap();
        assert!(
            matches!(auth, AnthropicAuth::Airlock(_)),
            "airlock config must yield the Airlock arm"
        );
        let broker = RunBroker::start_anthropic_with(auth, messages_url)
            .await
            .unwrap();

        // Sandbox: an unmodified client with only the opaque run bearer. The
        // reply streams back only if sandbox → broker → gateway → upstream all
        // held AND the gateway swapped the session token for the real credential.
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", broker.endpoint.base_url))
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-5","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("AIRLOCK-OK"),
            "custody path should stream the reply back: {body}"
        );
        // the run bearer the sandbox holds is neither the session token nor the credential.
        assert_ne!(broker.endpoint.run_bearer, "ref-seed");
    }

    #[tokio::test]
    async fn airlock_broker_refuses_a_gateway_whose_measurement_mismatches() {
        let upstream = airlock_mock_anthropic().await;
        let gateway_url = boot_airlock_gateway(&upstream).await; // measures 0x11×48

        // Pin a DIFFERENT audited image; the attestation gate must reject the
        // gateway before any session is established or credential spent.
        let refused = AnthropicAuth::airlock(AirlockConfig {
            kind: airlock::wire::CredentialKind::Claude,
            gateway: AirlockGateway::Local { url: gateway_url },
            trust: AirlockTrust::Attested {
                measurement: "22".repeat(attest::MRTD_LEN),
                attest: "snp".into(),
            },
            sub: "test-sub".into(),
            work: WorkRef::Direct,
            snp_product: None,
            snp_vcek: None,
            pccs_url: None,
        })
        .await;
        assert!(
            refused.is_err(),
            "a gateway whose measurement != the pinned audited image must be refused"
        );
    }
}
