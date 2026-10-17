//! The `/v1` bodies' bytes, which a client written apart from this crate must match.

use abi::Root;
use node::Preconfirm;
use noded::wire::{Status, Submit};

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

#[test]
fn a_status_carries_the_roles_table_and_a_reader_skips_no_row() {
    let status = Status {
        network: "net".into(),
        time: 1,
        block_time_ms: 2,
        epoch_length: 3,
        height: 4,
        tip: [5; 32],
        root: Root([6; 32]),
        epoch: 7,
        identity: vec![8],
        contract: 9,
        genesis: [10; 32],
        roles: abi::Roles::from_iter([("programs", "p"), ("ledger", "l"), ("halo", "h")]),
    };
    let decoded: Status = abi::decode(&abi::encode(&status)).unwrap();
    assert_eq!(decoded, status);
    assert_eq!(decoded.roles.get("ledger").map(String::as_str), Some("l"));
    assert_eq!(decoded.roles.get("egress"), None);
}
