// Core-owned Forge producer golden for the raw blob-page reply.
#[path = "fixtures/producer_goldens.rs"]
mod fixtures;

use forge::{BlobBytesReply, ForgeReply, decode_reply, encode_reply};

fn decode_hex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn blob_bytes_reply_matches_committed_bytes_and_decodes() {
    let reply = ForgeReply::BlobBytes(BlobBytesReply {
        rev: "0123456789abcdef0123456789abcdef01234567".into(),
        path: "assets/logo.bin".into(),
        b64: "AAEC/w==".into(),
        size: 4,
        eof: true,
    });
    let expected = decode_hex(fixtures::REPLY_BLOB_BYTES);
    assert_eq!(encode_reply(&reply), expected);
    assert_eq!(decode_reply(&expected).unwrap(), reply);
}
