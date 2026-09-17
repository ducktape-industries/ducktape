# Test fixtures

`entitlements.plist` — the entitlements a macOS bundle is signed with. The
bundle itself is built in ducktape-app; the signing lane here only needs a real
plist to hand `rcodesign`, so `src/sign.rs`'s tests and `tests/sign_route.rs`
read this copy.

## Attestation fixtures

`tdx_quote` + `tdx_quote_collateral.json` — a real Intel-signed TDX quote and
its PCS collateral, vendored from the `dcap-qvl` 0.5.2 crate's `sample/`
directory (MIT, Phala Network). They let the full DCAP verification chain
(PCK chain → pinned Intel root, TCB info, QE identity, quote signature) run
offline in `tests/verify_tdx.rs`.

Verification is pinned to `NOW = 1751624655`, the midpoint of the collateral's
tcb_info/qe_identity validity intersection (1750329147..1752920163) — the
fixture and its window are historical constants, so the pinned time stays valid
forever.

`snp_report_milan.bin` + `snp_vcek_milan.der` — a real AMD-signed SEV-SNP
attestation report (decoded from `report_milan.hex`) and its VCEK, vendored
from `virtee/sev` `tests/certs_data/` (Apache-2.0). `tests/verify_snp.rs`
verifies the report's full chain against the AMD Milan ARK/ASK builtins —
real-silicon SNP coverage without SNP hardware. (Chain freshness note: X.509
validity on the VCEK is not time-checked by the sev verifier; the chain is a
historical constant.)
