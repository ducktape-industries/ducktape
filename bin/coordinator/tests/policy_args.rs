//! The policy selector maps CLI flags to the right `AuthPolicy` variant, and
//! `--genesis-set` reads the PUBLIC valset out of a real `network.toml`.

use coordinator_bin::select_policy;
use nat_traversal::AuthPolicy;

use commonware_cryptography::{Signer as _, ed25519};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn default_policy_requires_proof_of_possession() {
    let default = select_policy(&[]).unwrap();
    assert!(matches!(default, AuthPolicy::Public));
}

#[test]
fn genesis_set_pins_the_validators_from_network_toml() {
    // Two real genesis pubkeys.
    let a = ed25519::PrivateKey::from_seed(1).public_key();
    let b = ed25519::PrivateKey::from_seed(2).public_key();

    // A network.toml-shaped fixture with extra fields the coordinator ignores.
    let toml = format!(
        "chain_id = \"ducktape#deadbeef\"\n\
         scheme = \"ed25519\"\n\
         coordination = \"private\"\n\
         validators = [\"{}\", \"{}\"]\n\
         bootstrap = []\n",
        hex(a.as_ref()),
        hex(b.as_ref()),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("network.toml");
    std::fs::write(&path, toml).unwrap();

    let policy = select_policy(&["--genesis-set".into(), path.to_str().unwrap().into()]).unwrap();
    match policy {
        AuthPolicy::Private { genesis_set, .. } => {
            assert_eq!(genesis_set.len(), 2);
            assert!(genesis_set.contains(&a));
            assert!(genesis_set.contains(&b));
        }
        other => panic!("expected Private, got {other:?}"),
    }
}

#[test]
fn genesis_set_missing_file_is_a_hard_error() {
    let err = select_policy(&["--genesis-set".into(), "/no/such/network.toml".into()]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// Write `body` to a fresh `network.toml` and return (tempdir, path). The dir is
/// returned so the caller keeps it alive for the duration of the test.
fn network_toml(body: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("network.toml");
    std::fs::write(&path, body).unwrap();
    let s = path.to_str().unwrap().to_string();
    (dir, s)
}

#[test]
fn genesis_set_with_no_value_is_a_hard_error_not_a_downgrade() {
    // A bare `--genesis-set` (or as the final token) must NOT silently fall
    // through to Public — an operator who meant Private but
    // whose templated path expanded to nothing gets an error, not a weaker
    // public policy.
    let err = select_policy(&["--genesis-set".into()]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // `--genesis-set` immediately followed by another flag is likewise valueless.
    let err = select_policy(&[
        "--genesis-set".into(),
        "--listen".into(),
        "0.0.0.0:1".into(),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn genesis_set_duplicate_validator_is_rejected() {
    // The same pubkey twice would be a silently-smaller valset — reject it.
    let a = ed25519::PrivateKey::from_seed(1).public_key();
    let (_dir, path) = network_toml(&format!(
        "validators = [\"{h}\", \"{h}\"]\n",
        h = hex(a.as_ref())
    ));
    let err = select_policy(&["--genesis-set".into(), path]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("duplicate"),
        "expected a duplicate-validator message, got: {err}"
    );
}

#[test]
fn genesis_set_non_hex_validator_is_rejected() {
    // Non-hex characters must not slip past the hex validator.
    let (_dir, path) = network_toml("validators = [\"zzzz\"]\n");
    let err = select_policy(&["--genesis-set".into(), path]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn genesis_set_odd_length_validator_is_rejected() {
    // Odd-length hex is not a whole byte string.
    let (_dir, path) = network_toml("validators = [\"abc\"]\n");
    let err = select_policy(&["--genesis-set".into(), path]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn genesis_set_wrong_key_length_is_rejected() {
    // Valid hex, but too few bytes to decode as an ed25519 public key.
    let (_dir, path) = network_toml("validators = [\"deadbeef\"]\n");
    let err = select_policy(&["--genesis-set".into(), path]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn genesis_set_malformed_toml_is_rejected() {
    // Syntactically broken toml surfaces with the "network.toml:" prefix.
    let (_dir, path) = network_toml("validators = [\n");
    let err = select_policy(&["--genesis-set".into(), path]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("network.toml:"),
        "expected the network.toml parse-error prefix, got: {err}"
    );
}
