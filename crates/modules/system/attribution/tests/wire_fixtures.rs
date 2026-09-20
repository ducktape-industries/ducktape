use attribution::{
    Actor, AttributionEvent, AttributionQuery, Change, ChangeKind, Source, encode_event,
    encode_query,
};
use sdk::Cause;

#[test]
fn owning_encoder_matches_raw_wire_fixtures() {
    assert_eq!(
        encode_query(&AttributionQuery::Relations {
            source: Source {
                module: "files".into(),
                kind: "file".into(),
                object: "f1".into(),
            },
        }),
        include_bytes!("fixtures/relations-query.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
    assert_eq!(
        encode_event(&AttributionEvent::Changed(Change {
            seq: 1,
            source: Source {
                module: "files".into(),
                kind: "file".into(),
                object: "f1".into(),
            },
            revision: 2,
            recipient: 7,
            reason: attribution::Reason::Ownership,
            kind: ChangeKind::Added,
            detail: vec![120],
            actor: Actor::System,
            cause: Cause::Direct,
            height: 3,
        })),
        include_bytes!("fixtures/changed-event.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
}
