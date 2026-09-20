// Core-owned identity producer goldens. These exercise the SDK wire encoder and
// the add-key preimage used by the module; expected bytes are committed fixtures.
#[path = "fixtures/producer_goldens.rs"]
mod fixtures;

use identity::{
    AccountView, Authorizer, Control, IdentityMsg, IdentityQuery, IdentityReply, KeyScheme,
    KeyView, ProgramStanding, add_key_preimage, encode_msg, encode_query, encode_reply,
};

fn decode_hex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn assert_golden(name: &str, actual: &[u8], expected: &str) -> Vec<u8> {
    let expected = decode_hex(expected);
    assert_eq!(actual, expected.as_slice(), "{name}");
    expected
}

fn key_view(scheme: KeyScheme, byte: u8, len: usize) -> KeyView {
    KeyView {
        scheme,
        pubkey: vec![byte; len],
        label: Some("phone".into()),
        added_at: 42,
    }
}

fn account(control: Control, keys: Vec<KeyView>) -> AccountView {
    AccountView {
        number: 7,
        name: "alice".into(),
        control,
        keys,
        avatar: Some("/avatars/a.png".into()),
        bio: Some("hello".into()),
        updated_at: 42,
    }
}

#[test]
fn messages_and_queries_match_committed_bytes_and_decode() {
    let create = IdentityMsg::Create {
        name: "alice".into(),
        scheme: KeyScheme::Ed25519,
    };
    let set_name = IdentityMsg::SetName {
        name: "alice-2".into(),
    };
    let remove_key = IdentityMsg::RemoveKey {
        key: vec![0x44; 32],
    };
    for (name, message, expected) in [
        ("create", create, fixtures::MSG_CREATE),
        ("set_name", set_name, fixtures::MSG_SET_NAME),
        ("remove_key", remove_key, fixtures::MSG_REMOVE_KEY),
    ] {
        let bytes = assert_golden(name, &encode_msg(&message), expected);
        assert_eq!(identity::decode_msg(&bytes).unwrap(), message);
    }

    for (name, scheme, byte, len, expected_msg, expected_preimage) in [
        (
            "ed25519",
            KeyScheme::Ed25519,
            0x22,
            32,
            fixtures::MSG_ADD_KEY_ED25519,
            fixtures::PREIMAGE_ED25519,
        ),
        (
            "secp256k1",
            KeyScheme::Secp256k1,
            0x23,
            33,
            fixtures::MSG_ADD_KEY_SECP256K1,
            fixtures::PREIMAGE_SECP256K1,
        ),
        (
            "secp256r1",
            KeyScheme::Secp256r1,
            0x24,
            33,
            fixtures::MSG_ADD_KEY_SECP256R1,
            fixtures::PREIMAGE_SECP256R1,
        ),
    ] {
        let message = IdentityMsg::AddKey {
            scheme,
            label: Some("phone".into()),
            authorizer: Authorizer {
                key: vec![0x11; 32],
                account: 7,
                expires_at: 999,
                proof: vec![0xaa, 0xbb],
            },
        };
        let bytes = assert_golden(
            &format!("add_key_{name}"),
            &encode_msg(&message),
            expected_msg,
        );
        assert_eq!(identity::decode_msg(&bytes).unwrap(), message);
        let preimage = add_key_preimage("dognet", scheme, &vec![byte; len], 3, 7, 999);
        assert_golden(&format!("preimage_{name}"), &preimage, expected_preimage);
    }

    for (name, query, expected) in [
        ("get", IdentityQuery::Get { number: 7 }, fixtures::QUERY_GET),
        (
            "keygen",
            IdentityQuery::KeyGen {
                key: vec![0x22; 32],
            },
            fixtures::QUERY_KEYGEN,
        ),
        (
            "of_key",
            IdentityQuery::OfKey {
                key: vec![0x22; 32],
            },
            fixtures::QUERY_OF_KEY,
        ),
    ] {
        let bytes = assert_golden(name, &encode_query(&query), expected);
        assert_eq!(identity::decode_query(&bytes).unwrap(), query);
    }
}

#[test]
fn account_views_cover_key_schemes_and_control_variants() {
    let keys = vec![
        key_view(KeyScheme::Ed25519, 0x22, 32),
        key_view(KeyScheme::Secp256k1, 0x23, 33),
        key_view(KeyScheme::Secp256r1, 0x24, 33),
    ];
    for (name, view, expected) in [
        ("keys", account(Control::Keys, keys), fixtures::REPLY_KEYS),
        (
            "program",
            account(
                Control::Program {
                    controller: 9,
                    executor: "files".into(),
                    generation: 4,
                    standing: ProgramStanding::Active,
                },
                Vec::new(),
            ),
            fixtures::REPLY_PROGRAM,
        ),
        (
            "revoked",
            account(Control::Revoked { controller: 9 }, Vec::new()),
            fixtures::REPLY_REVOKED,
        ),
    ] {
        let reply = IdentityReply::Account(Some(view));
        let bytes = assert_golden(name, &encode_reply(&reply), expected);
        assert_eq!(identity::decode_reply(&bytes).unwrap(), reply);
    }
}
