//! Real TEE quote verification — the client-side half of attestation.
//! TDX via dcap-qvl (Intel root CA pinned inside the crate); SEV-SNP via the
//! AMD VCEK chain (sev crate, crypto_nossl backend; ARK/ASK pinned from the
//! crate builtins). There is no mock: a quote that does not chain to Intel or
//! AMD silicon roots does not verify. Injecting non-default `TrustRoots` only
//! weakens the injector — production entry points construct pinned roots.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use sev::certs::snp::{ca, Certificate, Chain, Verifiable};
use sev::firmware::guest::AttestationReport;
use sev::parser::Decoder;

use crate::attest::{AttestMode, Measurement, REPORT_DATA_LEN};

/// The ARK+ASK pair a VCEK chains to, for a caller that supplies roots out of
/// band (`SnpRoots { ca, .. }`) instead of taking [`SnpRoots::amd`]'s
/// builtins: `CaChain::from_pem(ark, ask)`. Re-exported so no caller links
/// the `sev` crate for the one type.
pub use sev::certs::snp::ca::Chain as CaChain;

/// AMD EPYC generation whose pinned root keys the verifier trusts. Pinned by
/// the operator beside the measurement — a measurement only means something on
/// the platform generation it was audited for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnpProduct {
    Milan,
    Genoa,
    Turin,
}

impl SnpProduct {
    fn kds_segment(self) -> &'static str {
        match self {
            Self::Milan => "Milan",
            Self::Genoa => "Genoa",
            Self::Turin => "Turin",
        }
    }

    fn builtin_ca(self) -> Result<ca::Chain> {
        use sev::certs::snp::builtin::{genoa, milan, turin};
        let (ark, ask) = match self {
            Self::Milan => (milan::ark(), milan::ask()),
            Self::Genoa => (genoa::ark(), genoa::ask()),
            Self::Turin => (turin::ark(), turin::ask()),
        };
        Ok(ca::Chain { ark: ark.context("builtin ARK")?, ask: ask.context("builtin ASK")? })
    }
}

impl std::str::FromStr for SnpProduct {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "milan" => Ok(Self::Milan),
            "genoa" => Ok(Self::Genoa),
            "turin" => Ok(Self::Turin),
            other => bail!("unknown SNP product {other:?} (want milan|genoa|turin)"),
        }
    }
}

/// Where the per-chip VCEK leaf certificate comes from. Transport, not trust:
/// whatever this yields must still chain to the pinned ARK/ASK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VcekSource {
    /// Fetch from the AMD Key Distribution Service by chip id + reported TCB.
    Kds,
    /// A DER certificate supplied out of band (air-gapped operation, tests).
    Der(Vec<u8>),
}

#[derive(Clone)]
pub struct SnpRoots {
    pub product: SnpProduct,
    pub ca: ca::Chain,
    pub vcek: VcekSource,
}

impl SnpRoots {
    /// Production roots: AMD's builtin ARK/ASK for `product`.
    pub fn amd(product: SnpProduct, vcek: VcekSource) -> Result<Self> {
        Ok(Self { product, ca: product.builtin_ca()?, vcek })
    }
}

#[derive(Clone)]
pub struct TdxRoots {
    /// Collateral endpoint; `None` = Intel PCS. The root CA the chain must
    /// anchor to is pinned inside dcap-qvl regardless.
    pub pccs_url: Option<String>,
}

/// Which pinned trust anchor a quote must chain to. Plain data: this crate
/// never reads env or flags — each binary parses its own config ONCE at its
/// boundary into these typed values (the provider's `AirlockConfig`, the
/// bins' flag parsing) and hands them in.
#[derive(Clone)]
pub enum TrustRoots {
    Tdx(TdxRoots),
    /// Boxed: a CA chain is ~1 KiB and would dominate the enum size.
    Snp(Box<SnpRoots>),
}

/// Structurally parse a quote WITHOUT verifying any signature, returning the
/// embedded measurement (hex) and REPORTDATA. TOFU inspection only — the
/// `ducktape user cred inspect` flow that pins a measurement for later verified use.
/// Never a trust decision.
pub fn peek_measurement(
    mode: AttestMode,
    quote: &[u8],
) -> Result<(String, [u8; REPORT_DATA_LEN])> {
    match mode {
        AttestMode::Tdx => {
            let q = dcap_qvl::quote::Quote::parse(quote)
                .map_err(|e| anyhow!("parse TDX quote: {e:?}"))?;
            let td = q.report.as_td10().context("quote is not a TDX TD10 report")?;
            let mut rd = [0u8; REPORT_DATA_LEN];
            rd.copy_from_slice(&td.report_data[..REPORT_DATA_LEN]);
            Ok((hex::encode(td.mr_td), rd))
        }
        AttestMode::Snp => {
            let report = AttestationReport::decode(&mut &quote[..], ())
                .context("parse SEV-SNP attestation report")?;
            Ok((hex::encode(report.measurement), report.report_data))
        }
    }
}

/// Verify `quote` against pinned trust roots and the expected measurement and
/// return the bound REPORTDATA. The ONLY way a caller learns a seal_pk.
pub async fn verify_quote(
    quote: &[u8],
    expected: &Measurement,
    roots: &TrustRoots,
) -> Result<[u8; REPORT_DATA_LEN]> {
    match roots {
        TrustRoots::Tdx(t) => verify_tdx(quote, expected, t).await,
        TrustRoots::Snp(s) => verify_snp(quote, expected, s).await,
    }
}

// ===== Intel TDX ============================================================

async fn verify_tdx(
    quote: &[u8],
    expected: &Measurement,
    roots: &TdxRoots,
) -> Result<[u8; REPORT_DATA_LEN]> {
    use dcap_qvl::collateral::{CollateralClient, INTEL_PCS_URL};
    let pccs = roots.pccs_url.as_deref().unwrap_or(INTEL_PCS_URL);
    let collateral = CollateralClient::with_default_http(pccs)
        .map_err(|e| anyhow!("collateral client: {e:?}"))?
        .fetch(quote)
        .await
        .map_err(|e| anyhow!("fetch TDX collateral: {e:?}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    verify_tdx_at(quote, &collateral, now, expected)
}

/// TCB statuses accepted as "trustworthy enough to release a credential to".
/// Anything else (OutOfDate, ConfigurationNeeded, Revoked, …) fails closed
/// with the status named.
const TDX_OK_STATUSES: &[&str] = &["UpToDate", "SWHardeningNeeded"];

/// TD ATTRIBUTES bit 0 — the TUD group's DEBUG flag. Intel's own words: *"In TD
/// debug mode, the CPU state and private memory are accessible by the host
/// VMM."* A debug TD boots the SAME audited image, so MRTD is unchanged and the
/// quote chains to real silicon; the only thing that differs is that the
/// operator can read the credential straight out of guest memory, which is the
/// single property this whole crate exists to provide.
const TDX_ATTR_DEBUG: u8 = 0x01;

/// Pure verification against provided collateral at a given time — the
/// fixture-testable core. Full dcap chain: PCK -> Intel root (pinned inside
/// dcap-qvl), TCB info, QE identity, quote signature.
pub fn verify_tdx_at(
    quote: &[u8],
    collateral: &dcap_qvl::QuoteCollateralV3,
    now_secs: u64,
    expected: &Measurement,
) -> Result<[u8; REPORT_DATA_LEN]> {
    let verified = dcap_qvl::verify::rustcrypto::verify(quote, collateral, now_secs)
        .map_err(|e| anyhow!("dcap verify: {e:?}"))?;
    accept_tdx_report(&verified, expected)
}

/// Everything a chain-VERIFIED TDX quote must still satisfy before a credential
/// is released to it, and the only source of the REPORTDATA `verify_tdx_at`
/// returns — so this cannot be skipped without failing to compile.
///
/// Split out because the debug gate has no other honest test: flipping the
/// DEBUG bit in the quote BYTES breaks the quote signature, so a fixture test
/// doing that would go green on the wrong refusal. Driving this with a genuinely
/// verified report whose attributes are then mutated tests the gate itself.
///
/// The debug check is deliberately OURS even though dcap-qvl 0.5 also refuses a
/// debug TD by default (`QuoteVerifier::allow_debug` is `false`): that default
/// is a builder knob on a dependency, and "the credential is unreadable by the
/// host" is not a property to hold by someone else's default.
pub fn accept_tdx_report(
    verified: &dcap_qvl::verify::VerifiedReport,
    expected: &Measurement,
) -> Result<[u8; REPORT_DATA_LEN]> {
    if !TDX_OK_STATUSES.contains(&verified.status.as_str()) {
        bail!(
            "TDX TCB status {:?} not accepted (advisories: {:?})",
            verified.status,
            verified.advisory_ids
        );
    }
    let td = verified.report.as_td10().context("quote is not a TDX TD10 report")?;
    let td_is_debuggable = td.td_attributes[0] & TDX_ATTR_DEBUG != 0;
    if td_is_debuggable {
        bail!("TDX TD ATTRIBUTES set DEBUG: the host VMM can read this TD's private memory");
    }
    if td.mr_td != expected.0 {
        bail!(
            "MRTD mismatch: {} != expected {} (not the audited image)",
            hex::encode(td.mr_td),
            expected.to_hex()
        );
    }
    let mut rd = [0u8; REPORT_DATA_LEN];
    rd.copy_from_slice(&td.report_data[..REPORT_DATA_LEN]);
    Ok(rd)
}

// ===== AMD SEV-SNP ==========================================================

/// NOTE (deliberate asymmetry with TDX): there is no TCB-freshness gate here.
/// AMD keeps issuing VCEKs for older TCBs, so a chain-valid report can attest
/// firmware at a rolled-back patch level; pinning a minimum `reported_tcb` per
/// product is the follow-up hardening, tracked with the real-hardware
/// validation TODO. TDX gets its freshness gate for free from the collateral's
/// TCB status.
async fn verify_snp(
    quote: &[u8],
    expected: &Measurement,
    roots: &SnpRoots,
) -> Result<[u8; REPORT_DATA_LEN]> {
    let report = AttestationReport::decode(&mut &quote[..], ())
        .context("parse SEV-SNP attestation report")?;

    let signing_key = report.key_info.signing_key();
    if signing_key != 0 {
        bail!(
            "SEV-SNP report signed by a non-VCEK key (signing_key={signing_key}); \
             only VCEK is supported"
        );
    }

    let vcek_der = match &roots.vcek {
        VcekSource::Der(der) => der.clone(),
        VcekSource::Kds => fetch_vcek_kds(&report, roots.product).await?,
    };
    let vcek =
        Certificate::from_der(&vcek_der).map_err(|e| anyhow!("parse VCEK certificate: {e}"))?;

    let chain = Chain { ca: roots.ca.clone(), vek: vcek };
    (&chain, &report)
        .verify()
        .map_err(|e| anyhow!("SEV-SNP chain/report signature: {e}"))?;

    accept_snp_report(&report, expected)
}

/// Everything a chain-VERIFIED SEV-SNP report must still satisfy before a
/// credential is released to it, and the only source of the REPORTDATA
/// `verify_snp` returns — so this cannot be skipped without failing to compile.
///
/// Split out for the same reason as [`accept_tdx_report`]: `policy` and `vmpl`
/// live INSIDE the signed region, so mutating them in the report bytes breaks
/// the signature and any fixture test doing that would go green on the wrong
/// refusal. Both checks run AFTER the chain verify, so they are decisions on
/// authenticated fields rather than on whatever the caller sent.
///
/// Unlike TDX, nothing upstream does this for us: the `sev` crate's
/// `(&chain, &report).verify()` is a certificate-chain and report-signature
/// check and reads neither field.
pub fn accept_snp_report(
    report: &AttestationReport,
    expected: &Measurement,
) -> Result<[u8; REPORT_DATA_LEN]> {
    // A debug-allowed guest boots the SAME audited image — measurement
    // unchanged, VCEK chain intact — and lets the hypervisor read its register
    // state. The launch measurement is SNP's whole trust story here (there is
    // deliberately no TCB-freshness gate, see `verify_snp`), so a policy that
    // hands the host the guest's state voids it entirely.
    if report.policy.debug_allowed() {
        bail!("SEV-SNP guest policy allows DEBUG: the host can read this guest's state");
    }
    // A report requested at VMPL > 0 attests a LESS privileged level than the
    // one holding the seal key, so it says nothing about the code that would
    // receive the credential.
    if report.vmpl != 0 {
        bail!(
            "SEV-SNP report was requested at VMPL {}, not 0 (it does not describe the enclave)",
            report.vmpl
        );
    }
    if report.measurement != expected.0 {
        bail!(
            "SEV-SNP measurement mismatch: {} != expected {} (not the audited image)",
            hex::encode(report.measurement),
            expected.to_hex()
        );
    }
    Ok(report.report_data)
}

/// Fetch the VCEK from AMD KDS by product / chip id / reported TCB SPLs,
/// caching per URL (KDS rate-limits aggressively; a broker re-verifies the
/// same gateway across restarts). Never put the URL in an error — it carries
/// the chip id.
async fn fetch_vcek_kds(report: &AttestationReport, product: SnpProduct) -> Result<Vec<u8>> {
    let tcb = &report.reported_tcb;
    // Turin chips carry an 8-byte hwid in a 64-byte field.
    let hwid = match product {
        SnpProduct::Turin => hex::encode(&report.chip_id[..8]),
        _ => hex::encode(report.chip_id),
    };
    let mut url = format!(
        "https://kdsintf.amd.com/vcek/v1/{}/{}?blSPL={:02}&teeSPL={:02}&snpSPL={:02}&ucodeSPL={:02}",
        product.kds_segment(),
        hwid,
        tcb.bootloader,
        tcb.tee,
        tcb.snp,
        tcb.microcode
    );
    if let Some(fmc) = tcb.fmc {
        url.push_str(&format!("&fmcSPL={fmc:02}"));
    }

    static CACHE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(&url).cloned() {
        return Ok(hit);
    }

    // Drop the reqwest source: its Display can carry the URL (= chip id),
    // which must reach neither the log ring nor a CLI's stderr backtrace.
    let resp = reqwest::get(&url)
        .await
        .map_err(|_| anyhow!("fetch VCEK from AMD KDS: transport error (URL withheld)"))?;
    if !resp.status().is_success() {
        bail!("AMD KDS refused the VCEK request: HTTP {}", resp.status());
    }
    let der = resp.bytes().await.context("read VCEK body")?.to_vec();
    // Only cache a body that actually parses as a certificate — a transient
    // 200-with-garbage must not wedge every later verify until restart.
    Certificate::from_der(&der).map_err(|e| anyhow!("KDS returned a non-certificate body: {e}"))?;
    cache.lock().unwrap().insert(url, der.clone());
    Ok(der)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snp_product_parses_case_insensitively() {
        assert_eq!("milan".parse::<SnpProduct>().unwrap(), SnpProduct::Milan);
        assert_eq!("Genoa".parse::<SnpProduct>().unwrap(), SnpProduct::Genoa);
        assert_eq!("TURIN".parse::<SnpProduct>().unwrap(), SnpProduct::Turin);
        assert!("rome".parse::<SnpProduct>().is_err());
    }

}
