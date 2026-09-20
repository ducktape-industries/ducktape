use saga::{SagaMsg, encode_msg};

#[test]
fn owning_encoder_matches_raw_wire_fixture() {
    assert_eq!(
        encode_msg(&SagaMsg::Cancel {
            saga_id: "s1".into()
        }),
        include_bytes!("fixtures/cancel.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
}
