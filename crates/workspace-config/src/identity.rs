//! identity files — a persisted ed25519 secret, hex-encoded — plus the
//! user-key authorizer helpers the CLI mints and the chain-id mint.

use std::path::Path;

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Signer as _, ed25519};

use super::{hex_bytes, unhex};

/// load the identity at `path`, or generate one there from OS randomness.
/// returns the signer and whether it was freshly generated. written 0600 on
/// unix — this is the NODE's identity (mesh/valset/frame-signing key) only.
/// the user's identity is a separate keypair in the keystore
/// (`<workspace>/keys/<wallet>.key`), a member of an `identity` module
/// account (`crates/modules/system/identity`); no node is ever bound to an
/// account, and this file never holds the user key.
pub fn load_or_generate_identity(path: &Path) -> Result<(ed25519::PrivateKey, bool), String> {
    if path.exists() {
        return load_identity(path).map(|k| (k, false));
    }
    let key = generate_identity();
    write_identity(path, &key)?;
    Ok((key, true))
}

/// a fresh in-memory identity from OS randomness — for the caller whose
/// on-disk location is derived FROM the key (init's default workspace dir is
/// named by the chain id, which is minted from this pubkey). persist it with
/// [`write_identity`] once the destination exists.
pub fn generate_identity() -> ed25519::PrivateKey {
    let mut raw = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
    // every 32-byte string is a valid ed25519 seed (the scheme clamps), so
    // decode cannot fail on fresh OS randomness.
    ed25519::PrivateKey::decode(raw.as_slice()).expect("32 random bytes decode")
}

/// persist an identity at `path` with the same guarantees generation gives.
pub fn write_identity(path: &Path, key: &ed25519::PrivateKey) -> Result<(), String> {
    let encoded = hex_bytes(key.encode().as_ref());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    }
    // the file is BORN 0600 (no write-then-chmod window a co-tenant could
    // read the secret through), and create_new makes exists-then-create
    // race-free: a concurrent keygen loses cleanly instead of clobbering.
    {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(path)
            .map_err(|e| format!("create {path:?}: {e}"))?;
        if let Err(e) = f.write_all(format!("{encoded}\n").as_bytes()) {
            // a partial file would shadow every future load with a decode
            // error; remove it so the next run regenerates cleanly.
            let _ = std::fs::remove_file(path);
            return Err(format!("write {path:?}: {e}"));
        }
    }
    Ok(())
}

pub fn load_identity(path: &Path) -> Result<ed25519::PrivateKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let raw = unhex(text.trim()).map_err(|e| format!("{path:?}: {e}"))?;
    ed25519::PrivateKey::decode(raw.as_slice())
        .map_err(|e| format!("{path:?} is not an ed25519 secret: {e}"))
}

/// an existing ed25519 member's consent to admit `new_key` (of `scheme`) into
/// `account` at the new key's CURRENT generation on `chain_id`, dying at
/// `expires_at` -- the [`identity::Authorizer`] an `AddKey` carries. the CLI's
/// own key is always ed25519, so this is the one authorizer shape it mints;
/// the 64 signature bytes ARE the `KeyScheme::Ed25519` proof encoding.
pub fn ed25519_authorizer(
    user: &ed25519::PrivateKey,
    chain_id: &str,
    scheme: identity::KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
    expires_at: u64,
) -> identity::Authorizer {
    let preimage =
        identity::add_key_preimage(chain_id, scheme, new_key, generation, account, expires_at);
    identity::Authorizer {
        key: user.public_key().as_ref().to_vec(),
        account,
        expires_at,
        proof: user
            .sign(identity::IDENTITY_ADD_KEY_NS, &preimage)
            .as_ref()
            .to_vec(),
    }
}

/// refuse a network name the `duck://` address grammar cannot carry. The name
/// is the label of every chain id [`mint_chain_id`] mints from it, and sdk's
/// [`duck_address::ChainId`] is the ONE rule for a label: a name passes
/// exactly when a chain id labelled with it round-trips through that parser.
pub fn validate_network_name(name: &str) -> Result<(), duck_address::Refused> {
    let labelled = duck_address::ChainId {
        label: name.to_string(),
        salt: Default::default(),
    };
    labelled
        .to_string()
        .parse::<duck_address::ChainId>()
        .map(drop)
}

/// mint a chain-id: the human-readable name plus a short salt, so two
/// unrelated networks that pick the same name still get distinct namespaces
/// (their handshakes fail cleanly instead of colliding). the salt hashes the
/// initiator's identity and the wall clock — unique enough for a network id,
/// no rng plumbing needed.
pub fn mint_chain_id(name: &str, initiator: &ed25519::PublicKey) -> String {
    use commonware_cryptography::{Hasher as _, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is past the epoch")
        .as_nanos();
    let mut hasher = Sha256::default();
    hasher.update(initiator.as_ref());
    hasher.update(&nanos.to_le_bytes());
    let (_, digest) = hasher.finalize();
    format!("{name}#{}", hex_bytes(&digest.as_ref()[..4]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn identity_roundtrips_and_reuses() {
        let dir = tmp("identity");
        let path = dir.join("identity.key");
        let (a, generated) = load_or_generate_identity(&path).expect("generate");
        assert!(generated);
        let (b, generated) = load_or_generate_identity(&path).expect("reuse");
        assert!(
            !generated,
            "an existing identity is reused, never clobbered"
        );
        assert_eq!(a.public_key(), b.public_key());
    }

    // ---- add-key consents --------------------------------------------------

    const CHAIN: &str = "chain-a";
    const NEW_KEY: [u8; 32] = [9u8; 32];
    const ACCOUNT: u64 = 7;
    const EXPIRES: u64 = 900;

    /// the module's own check, verbatim: scheme-dispatched verify over
    /// `add_key_preimage` under the add-key namespace.
    fn consent_verifies(
        authorizer: &identity::Authorizer,
        chain_id: &str,
        new_key: &[u8],
        generation: u64,
        account: u64,
        expires_at: u64,
    ) -> bool {
        identity::KeyScheme::Ed25519.verify(
            &authorizer.key,
            identity::IDENTITY_ADD_KEY_NS,
            &identity::add_key_preimage(
                chain_id,
                identity::KeyScheme::Ed25519,
                new_key,
                generation,
                account,
                expires_at,
            ),
            &authorizer.proof,
        )
    }

    fn consent(generation: u64) -> identity::Authorizer {
        ed25519_authorizer(
            &ed25519::PrivateKey::from_seed(1),
            CHAIN,
            identity::KeyScheme::Ed25519,
            &NEW_KEY,
            generation,
            ACCOUNT,
            EXPIRES,
        )
    }

    #[test]
    fn ed25519_consent_verifies_against_the_module_preimage() {
        let user = ed25519::PrivateKey::from_seed(1);
        let authorizer = consent(0);
        assert_eq!(authorizer.key, user.public_key().as_ref());
        assert_eq!(authorizer.account, ACCOUNT);
        assert_eq!(authorizer.expires_at, EXPIRES);
        assert!(consent_verifies(
            &authorizer,
            CHAIN,
            &NEW_KEY,
            0,
            ACCOUNT,
            EXPIRES
        ));
    }

    #[test]
    fn ed25519_consent_is_chain_scoped() {
        assert!(!consent_verifies(
            &consent(0),
            "chain-b",
            &NEW_KEY,
            0,
            ACCOUNT,
            EXPIRES
        ));
    }

    #[test]
    fn ed25519_consent_is_scoped_to_its_generation_key_account_and_expiry() {
        let authorizer = consent(0);
        // the module advances the key's generation on admission, so a consent
        // signed at gen 0 never verifies at gen 1: single-use by construction.
        assert!(!consent_verifies(
            &authorizer,
            CHAIN,
            &NEW_KEY,
            1,
            ACCOUNT,
            EXPIRES
        ));
        assert!(!consent_verifies(
            &authorizer,
            CHAIN,
            &[10u8; 32],
            0,
            ACCOUNT,
            EXPIRES
        ));
        // and it admits into ONE account, until ONE time.
        assert!(!consent_verifies(
            &authorizer,
            CHAIN,
            &NEW_KEY,
            0,
            ACCOUNT + 1,
            EXPIRES
        ));
        assert!(!consent_verifies(
            &authorizer,
            CHAIN,
            &NEW_KEY,
            0,
            ACCOUNT,
            EXPIRES + 1
        ));
    }

    /// mirrors exactly what `ducktape account key add` mints: encode through
    /// `identity::encode_msg`, decode through `identity::decode_msg`, and check
    /// the message the module actually consumes round-trips as ONE json line.
    #[test]
    fn add_key_round_trips_through_identity_codec() {
        let user = ed25519::PrivateKey::from_seed(3);
        let msg = identity::IdentityMsg::AddKey {
            scheme: identity::KeyScheme::Ed25519,
            label: Some("laptop".into()),
            authorizer: ed25519_authorizer(
                &user,
                "test@abc",
                identity::KeyScheme::Ed25519,
                &NEW_KEY,
                2,
                ACCOUNT,
                EXPIRES,
            ),
        };
        let encoded = identity::encode_msg(&msg);
        assert_eq!(
            String::from_utf8(encoded.clone()).unwrap().lines().count(),
            1
        );
        assert_eq!(identity::decode_msg(&encoded).unwrap(), msg);
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_is_born_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tmp("perms");
        let path = dir.join("identity.key");
        load_or_generate_identity(&path).expect("generate");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "secret must never be world-readable, even transiently"
        );
    }

    /// a name the address grammar carries mints a chain id sdk's parser
    /// reads back; any other is refused by the parser's own reason token.
    #[test]
    fn a_network_name_is_a_label_the_address_grammar_carries() {
        let founder = ed25519::PrivateKey::from_seed(1).public_key();
        for good in ["dognet", "my-team-2", "0"] {
            assert_eq!(validate_network_name(good), Ok(()), "{good:?}");
            let minted = mint_chain_id(good, &founder);
            let parsed: duck_address::ChainId = minted.parse().expect("minted id parses");
            assert_eq!(parsed.label, good);
        }
        for bad in [
            "My Team", "Dognet", "my team", "my_team", "dog#net", "dog/net", "",
        ] {
            let refused = validate_network_name(bad).expect_err(bad);
            assert_eq!(refused.reason, "invalid_input", "{bad:?}");
        }
    }
}
