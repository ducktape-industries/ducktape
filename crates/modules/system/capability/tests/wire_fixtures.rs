use capability::{
    CapabilityMsg, CapabilityQuery, CapabilityReply, encode_msg, encode_query, encode_reply,
};

#[test]
fn owning_encoder_matches_raw_wire_fixtures() {
    assert_eq!(
        encode_query(&CapabilityQuery::Providers {
            capability: "codex".into(),
        }),
        include_bytes!("fixtures/providers-query.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
    assert_eq!(
        encode_reply(&CapabilityReply::Providers(vec![vec![1, 2], vec![3, 4]])),
        include_bytes!("fixtures/providers-reply.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
    assert_eq!(
        encode_msg(&CapabilityMsg::ClaimClass {
            class: "agent".into(),
        }),
        br#"{"claim_class":{"class":"agent"}}"#
    );
}
