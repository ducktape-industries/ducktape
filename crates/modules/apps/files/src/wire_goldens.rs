use super::attribution_contract as attribution;
use super::identity_contract as identity;
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
fn local_consumer_contracts_match_committed_json_goldens() {
    let query = identity::IdentityQuery::Get { number: 7 };
    assert_eq!(identity::encode_query(&query), fixture("identity_get.json"));
    assert_eq!(
        identity::decode_query(&fixture("identity_get.json")).unwrap(),
        query
    );

    let reply = identity::IdentityReply::Account(None);
    assert_eq!(
        identity::encode_reply(&reply),
        fixture("identity_none.json")
    );

    let message = attribution::AttributionMsg::Attribute {
        object: attribution::ObjectRef {
            kind: "blob".into(),
            object: "sha256:abc".into(),
        },
        revision: 4,
        actor: attribution::Actor::Account(7),
        relations: vec![attribution::Relation {
            recipient: 8,
            reason: attribution::Reason::Ownership,
            detail: vec![1, 2],
        }],
        transfers: Vec::new(),
    };
    assert_eq!(
        attribution::encode_msg(&message),
        fixture("attribution_attribute.json")
    );
}
