use super::Activations;

#[test]
fn reads_activate_for_duckpond_and_its_descendants_without_changing_other_gates() {
    let duckpond = Activations::of(b"duckpond", None);
    assert_eq!(duckpond.reads_from, 553015);
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
