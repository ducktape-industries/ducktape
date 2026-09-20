use crate::attribution_contract as attribution;
use crate::tracker;
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
fn local_follow_up_contracts_match_committed_json_goldens() {
    let channel = tracker::create_channel_msg("chat", "repo", 7);
    assert_eq!(channel.payload, fixture("chat_create_channel.json"));

    let line = tracker::system_line_msg("chat", "repo", 7, "state".into(), "closed");
    assert_eq!(line.payload, fixture("chat_post_message.json"));

    let attribution = attribution::AttributionMsg::AttributeBatch {
        updates: vec![attribution::AttributionUpdate {
            object: attribution::ObjectRef {
                kind: "ref".into(),
                object: "repo/main".into(),
            },
            revision: 3,
            actor: attribution::Actor::Account(7),
            relations: vec![attribution::Relation {
                recipient: 8,
                reason: attribution::Reason::Defined("ref_writer".into()),
                detail: vec![1, 2],
            }],
            transfers: Vec::new(),
        }],
    };
    assert_eq!(
        attribution::encode_msg(&attribution),
        fixture("attribution_batch.json")
    );
}
