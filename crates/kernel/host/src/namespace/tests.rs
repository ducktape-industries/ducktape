use super::Activations;

#[test]
fn reads_activate_for_duckpond_and_its_descendants_without_changing_other_gates() {
    let duckpond = Activations::of(b"duckpond", None);
    assert_eq!(duckpond.reads_from, 553015);
    let responder = duckpond.preparation_responder.unwrap();
    assert_eq!(responder.from, 563700);
    assert_eq!(
        responder.key,
        &[
            0x75, 0xbf, 0x26, 0x5d, 0x1e, 0x35, 0x10, 0x36, 0x77, 0xd0, 0x09, 0xf4, 0xfb, 0xba,
            0x04, 0x01, 0x99, 0x6c, 0x43, 0x7f, 0x56, 0xe0, 0x48, 0x1b, 0x3a, 0x9f, 0x9c, 0x1e,
            0x53, 0xaf, 0xe3, 0x7d
        ]
    );
    assert_eq!(duckpond.layout_from, u64::MAX);
    assert_eq!(duckpond.measured_depth_from, 700000);
    assert_eq!(Activations::of(b"lab", Some(b"duckpond")), duckpond);

    let sealpond = Activations::of(b"sealpond", None);
    assert_eq!(sealpond.reads_from, u64::MAX);
    assert_eq!(sealpond.layout_from, u64::MAX);
    assert_eq!(sealpond.measured_depth_from, u64::MAX);
    assert_eq!(Activations::of(b"lab", Some(b"sealpond")), sealpond);
    assert_eq!(Activations::of(b"independent", None).reads_from, 0);
}
