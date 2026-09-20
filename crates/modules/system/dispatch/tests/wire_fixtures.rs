use dispatch::saga_contract::{SagaMsg, encode_msg as encode_saga_msg};
use dispatch::{DispatchMsg, encode_msg};

#[test]
fn owning_encoders_match_raw_wire_fixtures() {
    assert_eq!(
        encode_msg(&DispatchMsg::CancelDispatch {
            dispatch_id: "d1".into(),
        }),
        include_bytes!("fixtures/cancel-dispatch.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
    assert_eq!(
        encode_saga_msg(&SagaMsg::Cancel {
            saga_id: "s1".into()
        }),
        include_bytes!("fixtures/saga-cancel.json")
            .strip_suffix(b"\n")
            .unwrap()
    );
}
