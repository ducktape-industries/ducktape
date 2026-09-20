use forge::{
    ForgeMsg, ForgeQuery, ForgeReply, channel_id_for, decode_msg, decode_query, decode_reply,
    encode_msg, encode_query, encode_reply,
};
use std::fs;

fn fixture(name: &str) -> Vec<u8> {
    let bytes = fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    bytes.strip_suffix(b"\n").unwrap_or(&bytes).to_vec()
}

#[test]
fn committed_wire_and_semantic_goldens_match_current_codecs() {
    let message = ForgeMsg::OpenIssue {
        repo: "repo".into(),
        title: "bug".into(),
        body: "fix".into(),
    };
    assert_eq!(encode_msg(&message), fixture("msg_open_issue.json"));
    assert_eq!(
        decode_msg(&fixture("msg_open_issue.json")).unwrap(),
        message
    );

    let query = ForgeQuery::GetItem {
        repo: "repo".into(),
        number: 7,
    };
    assert_eq!(encode_query(&query), fixture("query_get_item.json"));
    assert_eq!(
        decode_query(&fixture("query_get_item.json")).unwrap(),
        query
    );

    let reply = ForgeReply::Head(None);
    assert_eq!(encode_reply(&reply), fixture("reply_head_none.json"));
    assert_eq!(
        decode_reply(&fixture("reply_head_none.json")).unwrap(),
        reply
    );
    assert_eq!(channel_id_for("repo", 7), "forge:repo:7");
}
