//! Real Intel-signed TDX quote verified offline: the full dcap-qvl chain
//! (PCK chain -> Intel root, TCB info, QE identity, quote signature) runs
//! against vendored collateral at a pinned timestamp inside its validity
//! window. See fixtures/README.md for provenance.
#![cfg(feature = "verify")]

use airlock::attest::Measurement;
use airlock::verify::{accept_tdx_report, verify_tdx_at};

const QUOTE: &[u8] = include_bytes!("fixtures/tdx_quote");
const COLLATERAL: &[u8] = include_bytes!("fixtures/tdx_quote_collateral.json");
/// Midpoint of the collateral's tcb_info/qe_identity validity intersection.
const NOW: u64 = 1751624655;

fn collateral() -> dcap_qvl::QuoteCollateralV3 {
    serde_json::from_slice(COLLATERAL).unwrap()
}

fn quote_mrtd() -> Measurement {
    // Pin the fixture's own MRTD by parsing the (not yet verified) quote.
    use dcap_qvl::quote::Quote;
    let q = Quote::parse(QUOTE).unwrap();
    Measurement(q.report.as_td10().unwrap().mr_td)
}

#[test]
fn real_tdx_quote_verifies_against_pinned_intel_roots() {
    let rd = verify_tdx_at(QUOTE, &collateral(), NOW, &quote_mrtd()).unwrap();
    assert_eq!(rd.len(), 64);
}

#[test]
fn wrong_mrtd_is_rejected() {
    let wrong = Measurement([0x22; airlock::attest::MRTD_LEN]);
    assert!(verify_tdx_at(QUOTE, &collateral(), NOW, &wrong).is_err());
}

#[test]
fn tampered_quote_is_rejected() {
    let mut quote = QUOTE.to_vec();
    let mid = quote.len() / 2;
    quote[mid] ^= 1;
    assert!(verify_tdx_at(&quote, &collateral(), NOW, &quote_mrtd()).is_err());
}

#[test]
fn expired_collateral_is_rejected() {
    // A `now` far past nextUpdate must fail closed.
    let far_future = NOW + 10 * 365 * 24 * 3600;
    assert!(verify_tdx_at(QUOTE, &collateral(), far_future, &quote_mrtd()).is_err());
}

/// A TD running in DEBUG mode is refused even though everything else about it
/// verifies — same audited image, same MRTD, same Intel-signed chain — because
/// the host VMM can read a debug TD's private memory, which is the one property
/// sealing a credential into an enclave is supposed to buy.
///
/// The DEBUG bit is inside the signed quote, so flipping it in the BYTES would
/// break the signature and this test would go green on the wrong refusal.
/// Instead the REAL fixture is verified first and its attributes mutated after —
/// so the only thing that can refuse the second call is the gate itself.
#[test]
fn a_debug_mode_td_is_refused_although_the_honest_fixture_is_not() {
    use dcap_qvl::quote::Report;

    let verified = dcap_qvl::verify::rustcrypto::verify(QUOTE, &collateral(), NOW).unwrap();
    // The honest fixture: a real Intel-signed quote, DEBUG clear, still accepted.
    assert!(
        accept_tdx_report(&verified, &quote_mrtd()).is_ok(),
        "the real fixture must not be caught by the debug gate"
    );

    let mut debuggable = verified.clone();
    let Report::TD10(td) = &mut debuggable.report else {
        panic!("the TDX fixture is a TD10 report");
    };
    assert_eq!(
        td.td_attributes[0] & 0x01,
        0,
        "the fixture starts non-debug"
    );
    td.td_attributes[0] |= 0x01; // TUD.DEBUG
    let refused = accept_tdx_report(&debuggable, &quote_mrtd()).unwrap_err();
    assert!(
        refused.to_string().contains("DEBUG"),
        "a debug TD must be refused for BEING a debug TD, not incidentally: {refused}"
    );
}
