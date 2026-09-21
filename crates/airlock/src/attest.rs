//! Attestation: bind the enclave's seal + session public keys into a quote's
//! REPORTDATA, and let the client verify the quote before releasing a
//! credential.
//!
//! There is NO mock. Quote generation is vendor-generic via `configfs-tsm` (in
//! `airlock::server`): Intel TDX (`tdx_guest`) and AMD SEV-SNP (`sev_guest`)
//! share the sysfs report path. Verification is real and vendor-specific in
//! `airlock::verify` (feature `verify`). Non-TEE boxes test through
//! `airlock::testkit` (feature `testkit`), whose minted chains verify only
//! under their own roots.

use anyhow::{Context, Result, bail};

pub const REPORT_DATA_LEN: usize = 64;
/// Launch-measurement length: TDX MRTD and AMD SEV-SNP measurement are both
/// SHA-384 (48 bytes), so the SNP path reuses this too.
pub const MRTD_LEN: usize = 48;

/// The expected launch measurement (MRTD) of the audited host image. In the
/// PoC it is a CLI flag; production pins it on Ducktape consensus.
#[derive(Clone, Copy)]
pub struct Measurement(pub [u8; MRTD_LEN]);

impl Measurement {
    pub fn from_hex(s: &str) -> Result<Self> {
        let v = hex::decode(s.trim()).context("measurement is not valid hex")?;
        if v.len() != MRTD_LEN {
            bail!(
                "measurement must be {MRTD_LEN} bytes ({} hex chars), got {}",
                MRTD_LEN * 2,
                v.len()
            );
        }
        let mut m = [0u8; MRTD_LEN];
        m.copy_from_slice(&v);
        Ok(Measurement(m))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// The attestation vendor; each needs the matching confidential-VM silicon.
/// `auto` (a gateway-config value) resolves to `Tdx`/`Snp` by probing
/// `configfs-tsm`'s `provider`, so it is not a variant here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttestMode {
    Tdx,
    Snp,
}

impl AttestMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tdx => "tdx",
            Self::Snp => "snp",
        }
    }
}

impl std::str::FromStr for AttestMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tdx" => Ok(Self::Tdx),
            "snp" => Ok(Self::Snp),
            other => bail!("unknown attest mode {other:?} (want 'tdx' or 'snp')"),
        }
    }
}

/// REPORTDATA is exactly the two pubkeys concatenated (32 + 32 = 64), so the
/// quote *is* the key binding — no hashing, no trusting a side-channel field.
/// `seal_pk` is load-bearing today (credential seal + session handshake bind to
/// it). `sess_pk` binds the token-signing key into the SAME attestation so a
/// verifier could later check a token was signed by the attested enclave
/// offline; no current caller does, but it costs 32 bytes and completes the
/// binding.
pub fn make_report_data(seal_pk: &[u8; 32], sess_pk: &[u8; 32]) -> [u8; REPORT_DATA_LEN] {
    let mut rd = [0u8; REPORT_DATA_LEN];
    rd[..32].copy_from_slice(seal_pk);
    rd[32..].copy_from_slice(sess_pk);
    rd
}

pub fn split_report_data(rd: &[u8; REPORT_DATA_LEN]) -> ([u8; 32], [u8; 32]) {
    let mut seal_pk = [0u8; 32];
    let mut sess_pk = [0u8; 32];
    seal_pk.copy_from_slice(&rd[..32]);
    sess_pk.copy_from_slice(&rd[32..]);
    (seal_pk, sess_pk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_data_split_is_inverse() {
        let seal = [1u8; 32];
        let sess = [2u8; 32];
        let rd = make_report_data(&seal, &sess);
        assert_eq!(split_report_data(&rd), (seal, sess));
    }
}
