//! The `apple-codesign` credential: admission of a Developer ID signing
//! identity into the gateway's store.
//!
//! A [`CredentialPayload::AppleCodesign`](crate::wire::CredentialPayload)
//! arrives sealed and is opened ONCE, here, into an [`AppleCodesign`] the
//! gateway holds in memory. Nothing is stored that did not pass every check,
//! and nothing here is ever read back out over a route: the identity exists
//! only to be handed to a signer inside the same process.
//!
//! What is checked, and the snake_case token each refusal answers with:
//!
//! - `p12_unparseable` — the PKCS#12 is not one the `p12` crate opens under
//!   the given password (its MAC must verify), or it holds no certificate or
//!   no private key. The crate speaks PKCS#12 PBES1 (SHA-1 + 3DES / RC2), which
//!   is what Keychain Access exports and what `rcodesign --p12-file` opens; an
//!   `openssl pkcs12 -export` needs `-legacy` to produce the same.
//! - `not_developer_id_application` — the leaf certificate does not carry
//!   Apple's Developer ID Application marker extension
//!   ([`DEVELOPER_ID_APPLICATION_OID`]). The marker is the check, not the
//!   subject CN: it is the field Apple's own designated requirement
//!   (`certificate leaf[field.1.2.840.113635.100.6.1.13]`) keys on, and a CN is
//!   free text.
//! - `team_id_mismatch` — the leaf's subject OU (the Team ID on every Apple
//!   developer certificate) is not the `team_id` the upload names. The team is
//!   fixed at enrolment so a re-upload cannot quietly swap the signing team.
//! - `api_key_malformed` — the App Store Connect key is not the one-file JSON
//!   `rcodesign encode-app-store-connect-api-key` writes: an object with
//!   non-empty `key_id`, `issuer_id`, and a `private_key` holding a PEM
//!   `PRIVATE KEY` block.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use x509_cert::Certificate;
use x509_cert::der::Decode as _;
use x509_cert::der::asn1::{ObjectIdentifier, PrintableStringRef, Utf8StringRef};

/// Apple's certificate extension marking a Developer ID Application leaf.
pub const DEVELOPER_ID_APPLICATION_OID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113635.100.6.1.13");

/// X.520 `organizationalUnitName` — the Team ID on an Apple developer cert.
const OU_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.11");

/// Why an `apple-codesign` upload was refused. Each variant answers with one
/// stable token (see the module docs) and never echoes the material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    P12Unparseable,
    NotDeveloperIdApplication,
    TeamIdMismatch,
    ApiKeyMalformed,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P12Unparseable => "p12_unparseable",
            Self::NotDeveloperIdApplication => "not_developer_id_application",
            Self::TeamIdMismatch => "team_id_mismatch",
            Self::ApiKeyMalformed => "api_key_malformed",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Refusal {}

/// An admitted signing identity: exactly the bytes a signer is handed, kept
/// as they arrived so the PKCS#12 a later `rcodesign` run opens is the one
/// the checks ran on. No `Debug`: nothing here may reach a log line.
pub struct AppleCodesign {
    p12: Vec<u8>,
    p12_password: String,
    api_key_json: String,
    team_id: String,
}

impl AppleCodesign {
    /// Validate one upload's fields and hold them. Every refusal is a
    /// [`Refusal`] token; the order is the cheapest check first, so a
    /// malformed API key is named before the PKCS#12 is opened.
    pub fn admit(
        p12_b64: &str,
        p12_password: &str,
        api_key_json: &str,
        team_id: &str,
    ) -> Result<Self, Refusal> {
        check_api_key(api_key_json)?;
        let p12 = BASE64
            .decode(p12_b64)
            .map_err(|_| Refusal::P12Unparseable)?;
        let leaf = open_p12_leaf(&p12, p12_password)?;
        let cert = Certificate::from_der(&leaf).map_err(|_| Refusal::P12Unparseable)?;
        check_developer_id_application(&cert)?;
        check_team_id(&cert, team_id)?;
        Ok(Self {
            p12,
            p12_password: p12_password.to_string(),
            api_key_json: api_key_json.to_string(),
            team_id: team_id.to_string(),
        })
    }

    /// The PKCS#12 bytes as uploaded.
    pub fn p12(&self) -> &[u8] {
        &self.p12
    }

    pub fn p12_password(&self) -> &str {
        &self.p12_password
    }

    /// The App Store Connect key JSON as uploaded.
    pub fn api_key_json(&self) -> &str {
        &self.api_key_json
    }

    /// The Team ID the leaf certificate names (equal to the upload's).
    pub fn team_id(&self) -> &str {
        &self.team_id
    }
}

/// Open the PKCS#12 under `password` and return the leaf certificate's DER.
/// The MAC must verify, and the bundle must hold a certificate AND a private
/// key: a certificate alone cannot sign, and a key alone names no team.
fn open_p12_leaf(p12: &[u8], password: &str) -> Result<Vec<u8>, Refusal> {
    let pfx = p12::PFX::parse(p12).map_err(|_| Refusal::P12Unparseable)?;
    let mac_ok = pfx.verify_mac(password);
    if !mac_ok {
        return Err(Refusal::P12Unparseable);
    }
    let certs = pfx
        .cert_bags(password)
        .map_err(|_| Refusal::P12Unparseable)?;
    let keys = pfx
        .key_bags(password)
        .map_err(|_| Refusal::P12Unparseable)?;
    let has_key = !keys.is_empty();
    if !has_key {
        return Err(Refusal::P12Unparseable);
    }
    // The leaf is the first certificate bag: `PFX::new` (and Keychain's
    // export) write the identity's own certificate first, its chain after.
    certs.into_iter().next().ok_or(Refusal::P12Unparseable)
}

fn check_developer_id_application(cert: &Certificate) -> Result<(), Refusal> {
    let marked = cert
        .tbs_certificate
        .extensions
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|ext| ext.extn_id == DEVELOPER_ID_APPLICATION_OID);
    if !marked {
        return Err(Refusal::NotDeveloperIdApplication);
    }
    Ok(())
}

fn check_team_id(cert: &Certificate, team_id: &str) -> Result<(), Refusal> {
    let Some(ou) = subject_ou(cert) else {
        return Err(Refusal::TeamIdMismatch);
    };
    let matches = ou == team_id;
    if !matches {
        return Err(Refusal::TeamIdMismatch);
    }
    Ok(())
}

/// The first `OU=` of the subject, decoded as the string type it was encoded
/// as (Apple issues PrintableString; a UTF8String is equally an OU).
fn subject_ou(cert: &Certificate) -> Option<String> {
    cert.tbs_certificate
        .subject
        .0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .find(|attr| attr.oid == OU_OID)
        .and_then(|attr| {
            let printable = attr.value.decode_as::<PrintableStringRef<'_>>();
            if let Ok(value) = printable {
                return Some(value.to_string());
            }
            attr.value
                .decode_as::<Utf8StringRef<'_>>()
                .ok()
                .map(|value| value.to_string())
        })
}

fn check_api_key(api_key_json: &str) -> Result<(), Refusal> {
    let json: serde_json::Value =
        serde_json::from_str(api_key_json).map_err(|_| Refusal::ApiKeyMalformed)?;
    let field = |name: &str| {
        json.get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(Refusal::ApiKeyMalformed)
    };
    field("key_id")?;
    field("issuer_id")?;
    let private_key = field("private_key")?;
    let is_pem_private_key = private_key.contains("-----BEGIN PRIVATE KEY-----")
        && private_key.contains("-----END PRIVATE KEY-----");
    if !is_pem_private_key {
        return Err(Refusal::ApiKeyMalformed);
    }
    Ok(())
}

/// Throwaway Developer-ID-shaped identities for tests: a self-signed leaf
/// carrying the marker extension (or not) under a chosen OU, bundled with its
/// key as a PKCS#12. Behind `testkit` so no product build links the builder.
#[cfg(feature = "testkit")]
pub mod fixture {
    use std::sync::OnceLock;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use sha2::Sha256;
    use x509_cert::der::Encode as _;
    use x509_cert::der::asn1::{Null, ObjectIdentifier, OctetString};
    use x509_cert::ext::Extension;
    use x509_cert::spki::{EncodePublicKey as _, SubjectPublicKeyInfoOwned};

    use super::DEVELOPER_ID_APPLICATION_OID;

    /// The Team ID the fixture identity is minted under.
    pub const TEAM_ID: &str = "ABCDE12345";
    /// The password the fixture PKCS#12 is exported under.
    pub const P12_PASSWORD: &str = "fixture-pw";

    /// A well-formed App Store Connect key file (the key material is a
    /// throwaway PEM the gateway only checks the shape of).
    pub fn api_key_json() -> String {
        serde_json::json!({
            "key_id": "ABC123DEF4",
            "issuer_id": "11111111-2222-3333-4444-555555555555",
            "private_key": "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg\n-----END PRIVATE KEY-----\n",
        })
        .to_string()
    }

    /// Whether the minted leaf carries Apple's Developer ID Application marker.
    #[derive(Clone, Copy)]
    pub enum Marker {
        DeveloperIdApplication,
        None,
    }

    /// Mint one identity and export it as a base64 PKCS#12 under
    /// [`P12_PASSWORD`]: the `p12_b64` an upload carries.
    pub fn p12_b64(team_id: &str, marker: Marker) -> String {
        BASE64.encode(p12(team_id, marker))
    }

    /// The raw PKCS#12 bytes of a minted identity.
    pub fn p12(team_id: &str, marker: Marker) -> Vec<u8> {
        let key = signing_key();
        let cert = mint_leaf(team_id, marker, key);
        let key_der = rsa::pkcs8::EncodePrivateKey::to_pkcs8_der(key.as_ref())
            .expect("pkcs8")
            .as_bytes()
            .to_vec();
        p12::PFX::new(&cert, &key_der, None, P12_PASSWORD, "fixture")
            .expect("pfx")
            .to_der()
    }

    /// One RSA key for every fixture in the process: keygen is the slow part
    /// and nothing about a test depends on the key being fresh.
    fn signing_key() -> &'static rsa::pkcs1v15::SigningKey<Sha256> {
        static KEY: OnceLock<rsa::pkcs1v15::SigningKey<Sha256>> = OnceLock::new();
        KEY.get_or_init(|| {
            let private =
                rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("rsa keygen");
            rsa::pkcs1v15::SigningKey::<Sha256>::new(private)
        })
    }

    /// The marker's value is ASN.1 NULL, as Apple encodes it.
    fn marker_extension() -> Extension {
        let value = Null.to_der().expect("null der");
        Extension {
            extn_id: DEVELOPER_ID_APPLICATION_OID,
            critical: false,
            extn_value: OctetString::new(value).expect("octets"),
        }
    }

    fn mint_leaf(
        team_id: &str,
        marker: Marker,
        key: &rsa::pkcs1v15::SigningKey<Sha256>,
    ) -> Vec<u8> {
        use std::str::FromStr as _;
        use x509_cert::builder::{Builder, CertificateBuilder, Profile};
        use x509_cert::name::Name;
        use x509_cert::serial_number::SerialNumber;
        use x509_cert::time::Validity;

        let subject = Name::from_str(&format!(
            "CN=Developer ID Application: Ducktape Fixture ({team_id}),OU={team_id},O=Ducktape Fixture,C=US"
        ))
        .expect("subject dn");
        let public: &rsa::RsaPrivateKey = key.as_ref();
        let spki_der = public
            .to_public_key()
            .to_public_key_der()
            .expect("spki der");
        let spki = SubjectPublicKeyInfoOwned::try_from(spki_der.as_bytes()).expect("spki");
        let profile = Profile::Leaf {
            issuer: subject.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: false,
        };
        let mut builder = CertificateBuilder::new(
            profile,
            SerialNumber::from(7u32),
            Validity::from_now(std::time::Duration::from_secs(3600 * 24 * 365)).expect("validity"),
            subject,
            spki,
            key,
        )
        .expect("cert builder");
        match marker {
            Marker::DeveloperIdApplication => {
                builder
                    .add_extension(&RawExtension(marker_extension()))
                    .expect("marker ext");
            }
            Marker::None => {}
        }
        builder
            .build::<rsa::pkcs1v15::Signature>()
            .expect("sign cert")
            .to_der()
            .expect("cert der")
    }

    /// Adapter so a prebuilt [`Extension`] can go through the builder's
    /// `add_extension`, which wants an `AsExtension` implementor.
    struct RawExtension(Extension);

    impl x509_cert::der::Encode for RawExtension {
        fn encoded_len(&self) -> x509_cert::der::Result<x509_cert::der::Length> {
            self.0.extn_value.encoded_len()
        }

        fn encode(&self, encoder: &mut impl x509_cert::der::Writer) -> x509_cert::der::Result<()> {
            self.0.extn_value.encode(encoder)
        }
    }

    impl x509_cert::der::oid::AssociatedOid for RawExtension {
        const OID: ObjectIdentifier = DEVELOPER_ID_APPLICATION_OID;
    }

    impl x509_cert::ext::AsExtension for RawExtension {
        fn critical(&self, _subject: &x509_cert::name::Name, _extensions: &[Extension]) -> bool {
            false
        }

        fn to_extension(
            &self,
            _subject: &x509_cert::name::Name,
            _extensions: &[Extension],
        ) -> Result<Extension, x509_cert::der::Error> {
            Ok(self.0.clone())
        }
    }
}

#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::fixture::{self, Marker};
    use super::*;

    fn admit(p12_b64: &str, password: &str, api_key: &str, team: &str) -> Result<(), Refusal> {
        AppleCodesign::admit(p12_b64, password, api_key, team).map(|_| ())
    }

    #[test]
    fn a_developer_id_identity_under_its_team_is_admitted() {
        let p12 = fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication);
        let held = AppleCodesign::admit(
            &p12,
            fixture::P12_PASSWORD,
            &fixture::api_key_json(),
            fixture::TEAM_ID,
        )
        .expect("admitted");
        assert_eq!(held.team_id(), fixture::TEAM_ID);
        assert_eq!(held.p12_password(), fixture::P12_PASSWORD);
        assert_eq!(held.p12(), BASE64.decode(&p12).unwrap());
        assert_eq!(held.api_key_json(), fixture::api_key_json());
    }

    #[test]
    fn garbage_and_a_wrong_password_are_p12_unparseable() {
        let p12 = fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication);
        let key = fixture::api_key_json();
        assert_eq!(
            admit("not base64!", fixture::P12_PASSWORD, &key, fixture::TEAM_ID),
            Err(Refusal::P12Unparseable)
        );
        assert_eq!(
            admit(
                &BASE64.encode(b"not a pfx"),
                fixture::P12_PASSWORD,
                &key,
                fixture::TEAM_ID
            ),
            Err(Refusal::P12Unparseable)
        );
        assert_eq!(
            admit(&p12, "wrong-password", &key, fixture::TEAM_ID),
            Err(Refusal::P12Unparseable)
        );
    }

    #[test]
    fn a_leaf_without_the_marker_is_not_developer_id_application() {
        let p12 = fixture::p12_b64(fixture::TEAM_ID, Marker::None);
        assert_eq!(
            admit(
                &p12,
                fixture::P12_PASSWORD,
                &fixture::api_key_json(),
                fixture::TEAM_ID
            ),
            Err(Refusal::NotDeveloperIdApplication)
        );
    }

    #[test]
    fn another_teams_leaf_is_a_team_id_mismatch() {
        let p12 = fixture::p12_b64("ZZZZZ99999", Marker::DeveloperIdApplication);
        assert_eq!(
            admit(
                &p12,
                fixture::P12_PASSWORD,
                &fixture::api_key_json(),
                fixture::TEAM_ID
            ),
            Err(Refusal::TeamIdMismatch)
        );
    }

    #[test]
    fn an_api_key_missing_a_field_or_a_pem_block_is_malformed() {
        let p12 = fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication);
        let cases = [
            "not json",
            r#"{"key_id":"k","issuer_id":"i"}"#,
            r#"{"key_id":"","issuer_id":"i","private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----"}"#,
            r#"{"key_id":"k","issuer_id":"i","private_key":"MIGH...raw"}"#,
        ];
        for case in cases {
            assert_eq!(
                admit(&p12, fixture::P12_PASSWORD, case, fixture::TEAM_ID),
                Err(Refusal::ApiKeyMalformed),
                "{case}"
            );
        }
    }

    #[test]
    fn refusal_tokens_are_stable_snake_case() {
        assert_eq!(Refusal::P12Unparseable.to_string(), "p12_unparseable");
        assert_eq!(
            Refusal::NotDeveloperIdApplication.to_string(),
            "not_developer_id_application"
        );
        assert_eq!(Refusal::TeamIdMismatch.to_string(), "team_id_mismatch");
        assert_eq!(Refusal::ApiKeyMalformed.to_string(), "api_key_malformed");
    }
}
