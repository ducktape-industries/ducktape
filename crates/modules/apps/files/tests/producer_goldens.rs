// Core-owned Files producer goldens. The message/query/reply/write-output values
// are serde_json bytes; putblob is the separate tagged binary execute frame.
#[path = "fixtures/producer_goldens.rs"]
mod fixtures;

use std::collections::BTreeMap;

use files::{
    Actor, Change, Content, FilesMsg, FilesQuery, FilesReply, FilesWriteOutput, WriteOutcome,
    decode_msg, decode_query, decode_reply, decode_write_output, encode_msg, encode_putblob,
    encode_query, encode_reply, encode_write_output,
};

fn decode_hex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn golden(name: &str, actual: &[u8], expected: &str) -> Vec<u8> {
    let expected = decode_hex(expected);
    assert_eq!(actual, expected.as_slice(), "{name}");
    expected
}

fn message() -> FilesMsg {
    let mut meta = BTreeMap::new();
    meta.insert("content-type".into(), "text/plain".into());
    FilesMsg::Commit {
        base_snapshot: None,
        message: "first".into(),
        changes: vec![Change::Put {
            path: "/shared/readme.txt".into(),
            exec: false,
            meta,
            content: Content::Inline {
                b64: "aGVsbG8=".into(),
            },
        }],
    }
}

#[test]
fn json_boundaries_match_committed_bytes_and_decode() {
    let message = message();
    let query = FilesQuery::Read {
        path: "/shared/readme.txt".into(),
        snapshot: Some("a".repeat(64)),
        offset: 2,
        len: 3,
    };
    let reply = FilesReply::Read {
        b64: "bG8=".into(),
        eof: true,
    };
    let output = FilesWriteOutput {
        actor: Actor::Account(7),
        source_revision: 2,
        outcome: WriteOutcome::Commit {
            snapshot: "b".repeat(64),
        },
    };

    let message_bytes = golden(
        "commit_message",
        &encode_msg(&message),
        fixtures::MSG_COMMIT,
    );
    let query_bytes = golden("read_query", &encode_query(&query), fixtures::QUERY_READ);
    let reply_bytes = golden("read_reply", &encode_reply(&reply), fixtures::REPLY_READ);
    let output_bytes = golden(
        "write_output",
        &encode_write_output(&output),
        fixtures::WRITE_OUTPUT_COMMIT,
    );
    assert_eq!(message_bytes[0], b'{');
    assert_eq!(query_bytes[0], b'{');
    assert_eq!(reply_bytes[0], b'{');
    assert_eq!(output_bytes[0], b'{');
    assert_eq!(decode_msg(&message_bytes).unwrap(), message);
    assert_eq!(decode_query(&query_bytes).unwrap(), query);
    assert_eq!(decode_reply(&reply_bytes).unwrap(), reply);
    assert_eq!(decode_write_output(&output_bytes).unwrap(), output);
}

#[test]
fn putblob_frame_matches_committed_binary_bytes() {
    assert_eq!(
        encode_putblob(&[0, 1, 2, 255]),
        decode_hex(fixtures::PUTBLOB)
    );
}
