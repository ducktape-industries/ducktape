//! The `/v1` bodies' bytes, which a client written apart from this crate must match.

use node::Preconfirm;
use noded::wire::Submit;

#[test]
fn a_submit_is_its_frame_then_one_preconfirm_byte() {
    let submit = |preconfirm| {
        abi::encode(&Submit {
            frame: vec![7],
            preconfirm,
        })
    };
    assert_eq!(submit(Preconfirm::Off), [1, 0, 0, 0, 7, 0]);
    assert_eq!(submit(Preconfirm::On), [1, 0, 0, 0, 7, 1]);
}
