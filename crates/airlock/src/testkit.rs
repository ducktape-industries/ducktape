//! Fake silicon for tests: mints an SNP-format quote signed by a freshly
//! generated test chain. Verifies ONLY under `self.roots()` — never under the
//! AMD builtins — so nothing here can forge a quote a production verifier
//! accepts. RSA-2048 stands in for AMD's 4096-bit CA keys: the verifier checks
//! the algorithm (RSA-PSS/SHA-384), not the size, and 2048 keeps keygen fast.
//!
//! Also the fake NODE PROXY ([`behind_gateway_proxy`]), for the same reason: a
//! test that dials a lending gateway's listener directly is testing a topology
//! production does not have.

use anyhow::{anyhow, Context, Result};
use p384::ecdsa::signature::DigestSigner;
use sev::certs::snp::{ca, Certificate};
use sev::firmware::guest::AttestationReport;
use sev::parser::{Decoder, Encoder};
use sha2::{Digest, Sha384};

use crate::attest::{Measurement, REPORT_DATA_LEN};
use crate::verify::{SnpProduct, SnpRoots, TrustRoots, VcekSource};

/// SNP report byte-layout facts the minting relies on (AMD SEV-SNP ABI):
/// the signature covers bytes [0, 0x2a0); REPORT_DATA sits at 0x50.
const SIGNED_LEN: usize = 0x2a0;

pub struct SnpTestEnclave {
    ca: ca::Chain,
    vcek_der: Vec<u8>,
    signer: p384::ecdsa::SigningKey,
    measurement: Measurement,
}

impl SnpTestEnclave {
    pub fn new(measurement: &Measurement) -> Result<Self> {
        let mut rng = rand::rngs::OsRng;
        let ark_key = rsa::pss::SigningKey::<Sha384>::new(
            rsa::RsaPrivateKey::new(&mut rng, 2048).context("ARK keygen")?,
        );
        let ask_key = rsa::pss::SigningKey::<Sha384>::new(
            rsa::RsaPrivateKey::new(&mut rng, 2048).context("ASK keygen")?,
        );
        let vcek_key = p384::ecdsa::SigningKey::random(&mut rng);

        let ark_der = mint_cert(CertRole::Root, "CN=test ARK", rsa_spki(&ark_key)?, &ark_key)?;
        let ask_der =
            mint_cert(CertRole::SubCa { issuer: "CN=test ARK" }, "CN=test ASK", rsa_spki(&ask_key)?, &ark_key)?;
        let vcek_der =
            mint_cert(CertRole::Leaf { issuer: "CN=test ASK" }, "CN=test VCEK", p384_spki(&vcek_key)?, &ask_key)?;

        Ok(Self {
            ca: ca::Chain {
                ark: Certificate::from_der(&ark_der).map_err(|e| anyhow!("ARK parse: {e}"))?,
                ask: Certificate::from_der(&ask_der).map_err(|e| anyhow!("ASK parse: {e}"))?,
            },
            vcek_der,
            signer: vcek_key,
            measurement: *measurement,
        })
    }

    /// Mint a v2 SNP report carrying `report_data`, signed by the test VCEK.
    pub fn quote(&self, report_data: &[u8; REPORT_DATA_LEN]) -> Result<Vec<u8>> {
        let mut report = AttestationReport {
            version: 2,
            sig_algo: 1,          // ECDSA P-384 with SHA-384
            chip_id: [1u8; 64],   // non-Turin-like -> legacy TCB layout
            measurement: self.measurement.0,
            report_data: *report_data,
            ..Default::default()
        };

        let mut bytes = Vec::new();
        report.encode(&mut bytes, ()).context("encode unsigned report")?;

        let digest = Sha384::new_with_prefix(&bytes[..SIGNED_LEN]);
        let sig: p384::ecdsa::Signature = self.signer.sign_digest(digest);
        report.signature =
            sev::certs::snp::ecdsa::Signature::new(le72(&sig.r().to_bytes()), le72(&sig.s().to_bytes()));

        let mut out = Vec::new();
        report.encode(&mut out, ()).context("encode signed report")?;
        // The signed prefix must be unchanged by the re-encode, and the minted
        // bytes must survive the real parser.
        debug_assert_eq!(&out[..SIGNED_LEN], &bytes[..SIGNED_LEN]);
        AttestationReport::decode(&mut &out[..], ()).context("minted report must re-parse")?;
        Ok(out)
    }

    /// A server-injectable quoter minting quotes bound to the live REPORTDATA.
    /// Takes `Arc<Self>` because the server keeps the closure for its lifetime.
    #[cfg(feature = "server")]
    pub fn quoter(self: &std::sync::Arc<Self>) -> crate::server::Quoter {
        let enclave = self.clone();
        Box::new(move |rd| enclave.quote(rd))
    }

    /// The test chain as the PEM files an operator would pass `cred seal`
    /// out of band (`--snp-ark`, `--snp-ask`, `--snp-vcek`): ARK, ASK, and
    /// the VCEK as DER.
    pub fn root_files(&self) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let ark = self.ca.ark.to_pem().map_err(|e| anyhow!("ARK pem: {e}"))?;
        let ask = self.ca.ask.to_pem().map_err(|e| anyhow!("ASK pem: {e}"))?;
        Ok((ark, ask, self.vcek_der.clone()))
    }

    /// Trust roots under which — and only under which — `quote()` verifies.
    pub fn roots(&self) -> TrustRoots {
        TrustRoots::Snp(Box::new(SnpRoots {
            product: SnpProduct::Genoa,
            ca: self.ca.clone(),
            vcek: VcekSource::Der(self.vcek_der.clone()),
        }))
    }
}

/// Put an airlock router behind a stand-in for the node's gateway proxy.
///
/// In production nothing reaches a lending gateway's loopback listener except
/// through `bin/node`'s gateway plane, which authenticates the WireGuard peer
/// and stamps its node key as [`crate::server::CALLER_NODE_HEADER`] — refusing
/// any caller-supplied copy at decode. `node` is therefore what the proxy
/// VERIFIED, not what anyone asked for.
///
/// This is the ONLY way a test supplies a caller, because it is the only way
/// production does. A session request carries no identity — the field a
/// caller could once name itself with is deleted — so an ungranted member is
/// driven by wrapping with the node the proxy would really have stamped for
/// them, and asserting the lender refuses it.
#[cfg(feature = "server")]
pub fn behind_gateway_proxy(app: axum::Router, node: &[u8]) -> axum::Router {
    let stamped: axum::http::HeaderValue =
        hex::encode(node).parse().expect("hex is a valid header value");
    app.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let stamped = stamped.clone();
            async move {
                request
                    .headers_mut()
                    .insert(crate::server::CALLER_NODE_HEADER, stamped);
                next.run(request).await
            }
        },
    ))
}

/// 48-byte big-endian scalar -> the report's 72-byte little-endian field.
fn le72(be: &[u8]) -> [u8; 72] {
    let mut out = [0u8; 72];
    for (i, b) in be.iter().rev().enumerate() {
        out[i] = *b;
    }
    out
}

fn rsa_spki(key: &rsa::pss::SigningKey<Sha384>) -> Result<x509_cert::spki::SubjectPublicKeyInfoOwned> {
    use rsa::signature::Keypair;
    use x509_cert::spki::EncodePublicKey;
    let der = key.verifying_key().to_public_key_der().context("RSA SPKI")?;
    x509_cert::spki::SubjectPublicKeyInfoOwned::try_from(der.as_bytes())
        .map_err(|e| anyhow!("RSA SPKI parse: {e}"))
}

fn p384_spki(key: &p384::ecdsa::SigningKey) -> Result<x509_cert::spki::SubjectPublicKeyInfoOwned> {
    use x509_cert::spki::EncodePublicKey;
    let der = key.verifying_key().to_public_key_der().context("P-384 SPKI")?;
    x509_cert::spki::SubjectPublicKeyInfoOwned::try_from(der.as_bytes())
        .map_err(|e| anyhow!("P-384 SPKI parse: {e}"))
}

/// Which X.509 profile a test cert takes; extensions beyond the signature are
/// irrelevant to the sev verifier, which checks the RSA-PSS chain only.
enum CertRole {
    Root,
    SubCa { issuer: &'static str },
    Leaf { issuer: &'static str },
}

fn mint_cert(
    role: CertRole,
    subject: &str,
    spki: x509_cert::spki::SubjectPublicKeyInfoOwned,
    signer: &rsa::pss::SigningKey<Sha384>,
) -> Result<Vec<u8>> {
    use std::str::FromStr;
    use x509_cert::builder::{Builder, CertificateBuilder, Profile};
    use x509_cert::der::Encode;
    use x509_cert::name::Name;
    use x509_cert::serial_number::SerialNumber;
    use x509_cert::time::Validity;

    let name = |s: &str| Name::from_str(s).with_context(|| format!("DN {s}"));
    let profile = match role {
        CertRole::Root => Profile::Root,
        CertRole::SubCa { issuer } => {
            Profile::SubCA { issuer: name(issuer)?, path_len_constraint: None }
        }
        CertRole::Leaf { issuer } => Profile::Leaf {
            issuer: name(issuer)?,
            enable_key_agreement: false,
            enable_key_encipherment: false,
        },
    };
    let builder = CertificateBuilder::new(
        profile,
        SerialNumber::from(1u32),
        Validity::from_now(std::time::Duration::from_secs(3600 * 24 * 365)).context("validity")?,
        name(subject)?,
        spki,
        signer,
    )
    .context("cert builder")?;
    let cert = builder
        .build_with_rng::<rsa::pss::Signature>(&mut rand::rngs::OsRng)
        .map_err(|e| anyhow!("sign cert: {e}"))?;
    cert.to_der().context("cert DER")
}
