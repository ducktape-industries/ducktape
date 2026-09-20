use identity::{
    IdentityMsg, IdentityQuery, IdentityReply, KeyScheme, account_principal, add_key_preimage,
    decode_msg, decode_query, decode_reply, encode_msg, encode_query, encode_reply,
    principal_account,
};
use std::fs;

fn fixture(name: &str) -> Vec<u8> {
    let bytes = fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    if name.ends_with(".json") {
        bytes.strip_suffix(b"\n").unwrap_or(&bytes).to_vec()
    } else {
        bytes
    }
}

fn hex_fixture(name: &str) -> Vec<u8> {
    let text = String::from_utf8(fixture(name)).unwrap();
    text.trim()
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |byte| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("non-hex fixture byte"),
            };
            nibble(pair[0]) << 4 | nibble(pair[1])
        })
        .collect()
}

#[test]
fn committed_wire_and_semantic_goldens_match_current_codecs() {
    let msg = IdentityMsg::Create {
        name: "alice".into(),
        scheme: KeyScheme::Ed25519,
    };
    assert_eq!(encode_msg(&msg), fixture("msg_create.json"));
    assert_eq!(decode_msg(&fixture("msg_create.json")).unwrap(), msg);

    let query = IdentityQuery::OfKey { key: vec![1, 2, 3] };
    assert_eq!(encode_query(&query), fixture("query_of_key.json"));
    assert_eq!(decode_query(&fixture("query_of_key.json")).unwrap(), query);

    let reply = IdentityReply::Account(None);
    assert_eq!(encode_reply(&reply), fixture("reply_account_none.json"));
    assert_eq!(
        decode_reply(&fixture("reply_account_none.json")).unwrap(),
        reply
    );

    let preimage = add_key_preimage("net-a", KeyScheme::Ed25519, &[2; 32], 0, 1, 500);
    assert_eq!(preimage, hex_fixture("add_key_preimage.hex"));
    assert_eq!(account_principal(7), hex_fixture("account_principal_7.hex"));
    assert_eq!(
        principal_account(&hex_fixture("account_principal_7.hex")),
        Some(7)
    );
}
