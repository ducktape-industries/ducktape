//! Real SEV-SNP verification exercised end-to-end on non-TEE hardware: the
//! testkit mints a chain + report, and `airlock::verify` runs the REAL parser,
//! certificate-chain verify, and report-signature verify against it.
#![cfg(feature = "testkit")]

use airlock::attest::{self, MRTD_LEN, Measurement};
use airlock::testkit::SnpTestEnclave;
use airlock::verify::verify_quote;

fn meas(b: u8) -> Measurement {
    Measurement([b; MRTD_LEN])
}

#[tokio::test]
async fn minted_snp_quote_verifies_and_binds_report_data() {
    let enclave = SnpTestEnclave::new(&meas(0x11)).unwrap();
    let rd = attest::make_report_data(&[9u8; 32], &[8u8; 32]);
    let quote = enclave.quote(&rd).unwrap();
    let out = verify_quote(&quote, &meas(0x11), &enclave.roots())
        .await
        .unwrap();
    assert_eq!(out, rd);
}

#[tokio::test]
async fn wrong_measurement_is_rejected() {
    let enclave = SnpTestEnclave::new(&meas(0x11)).unwrap();
    let quote = enclave
        .quote(&attest::make_report_data(&[9u8; 32], &[8u8; 32]))
        .unwrap();
    assert!(
        verify_quote(&quote, &meas(0x22), &enclave.roots())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tampered_report_data_breaks_the_signature() {
    let enclave = SnpTestEnclave::new(&meas(0x11)).unwrap();
    let mut quote = enclave
        .quote(&attest::make_report_data(&[9u8; 32], &[8u8; 32]))
        .unwrap();
    quote[0x50] ^= 1; // REPORT_DATA offset in the SNP report
    assert!(
        verify_quote(&quote, &meas(0x11), &enclave.roots())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_quote_from_a_different_chain_is_rejected() {
    // Two independently minted enclaves: A's roots must refuse B's quote.
    let a = SnpTestEnclave::new(&meas(0x11)).unwrap();
    let b = SnpTestEnclave::new(&meas(0x11)).unwrap();
    let quote = b
        .quote(&attest::make_report_data(&[9u8; 32], &[8u8; 32]))
        .unwrap();
    assert!(verify_quote(&quote, &meas(0x11), &a.roots()).await.is_err());
}

#[tokio::test]
async fn garbage_quote_is_rejected() {
    let enclave = SnpTestEnclave::new(&meas(0x11)).unwrap();
    assert!(
        verify_quote(&[0u8; 32], &meas(0x11), &enclave.roots())
            .await
            .is_err()
    );
}

// ===== real AMD-signed fixture (vendored from virtee/sev test data) =========

use airlock::verify::{SnpProduct, SnpRoots, TrustRoots, VcekSource};

const MILAN_REPORT: &[u8] = include_bytes!("fixtures/snp_report_milan.bin");
const MILAN_VCEK: &[u8] = include_bytes!("fixtures/snp_vcek_milan.der");
/// MEASUREMENT offset in the SNP report (AMD SEV-SNP ABI, Table 22).
const MEASUREMENT_OFF: usize = 0x90;

fn milan_measurement() -> Measurement {
    let mut m = [0u8; MRTD_LEN];
    m.copy_from_slice(&MILAN_REPORT[MEASUREMENT_OFF..MEASUREMENT_OFF + MRTD_LEN]);
    Measurement(m)
}

fn milan_roots(product: SnpProduct) -> TrustRoots {
    TrustRoots::Snp(Box::new(
        SnpRoots::amd(product, VcekSource::Der(MILAN_VCEK.to_vec())).unwrap(),
    ))
}

#[tokio::test]
async fn real_amd_signed_milan_report_verifies_against_builtin_roots() {
    let rd = verify_quote(
        MILAN_REPORT,
        &milan_measurement(),
        &milan_roots(SnpProduct::Milan),
    )
    .await
    .unwrap();
    assert_eq!(rd.len(), 64);
}

#[tokio::test]
async fn milan_report_is_rejected_under_genoa_roots() {
    // The right product generation is part of the pinned trust: Genoa's
    // ARK/ASK must refuse the Milan VCEK.
    let err = verify_quote(
        MILAN_REPORT,
        &milan_measurement(),
        &milan_roots(SnpProduct::Genoa),
    )
    .await;
    assert!(err.is_err());
}

/// The SNP twin of the TDX debug gate, and the one that was genuinely open: the
/// `sev` crate's chain/report verify reads neither `policy` nor `vmpl`, so a
/// debug-allowed guest booting the AUDITED image produced a report that chained
/// to real AMD silicon, matched the pinned measurement, and handed the host the
/// guest's register state.
///
/// Both fields sit inside the signed region, so mutating the report BYTES would
/// break the signature and this test would go green on the wrong refusal. The
/// real AMD-signed fixture is decoded first and mutated after, so only the gate
/// itself can refuse the mutated cases.
#[test]
fn a_debug_or_non_vmpl0_snp_report_is_refused_although_the_honest_fixture_is_not() {
    use airlock::verify::accept_snp_report;
    use sev::firmware::guest::AttestationReport;
    use sev::parser::Decoder;

    let report = AttestationReport::decode(&mut &MILAN_REPORT[..], ()).unwrap();
    // The honest fixture: real AMD-signed, policy DEBUG clear, VMPL 0.
    assert!(
        accept_snp_report(&report, &milan_measurement()).is_ok(),
        "the real AMD fixture must not be caught by these gates"
    );
    assert!(
        !report.policy.debug_allowed(),
        "the fixture starts non-debug"
    );
    assert_eq!(report.vmpl, 0, "the fixture starts at VMPL 0");

    let mut debuggable = report;
    debuggable.policy.set_debug_allowed(true);
    let refused = accept_snp_report(&debuggable, &milan_measurement()).unwrap_err();
    assert!(
        refused.to_string().contains("DEBUG"),
        "a debug-allowed guest must be refused for THAT: {refused}"
    );

    let mut nested = report;
    nested.vmpl = 1;
    let refused = accept_snp_report(&nested, &milan_measurement()).unwrap_err();
    assert!(
        refused.to_string().contains("VMPL"),
        "a report from VMPL != 0 describes other code, and must say so: {refused}"
    );
}

#[tokio::test]
async fn tampered_real_report_is_rejected() {
    let mut quote = MILAN_REPORT.to_vec();
    quote[MEASUREMENT_OFF] ^= 1;
    // Measurement check aside, the signature over [0, 0x2a0) must break: use
    // the tampered measurement as the EXPECTED one so only the signature gates.
    let mut m = milan_measurement().0;
    m[0] ^= 1;
    let err = verify_quote(&quote, &Measurement(m), &milan_roots(SnpProduct::Milan)).await;
    assert!(err.is_err());
}
