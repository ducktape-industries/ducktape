use super::*;

fn starts(of: &[(&str, u64)]) -> BTreeMap<ProgramId, u64> {
    of.iter()
        .map(|(program, start)| (ProgramId::from(*program), *start))
        .collect()
}

#[test]
fn each_program_keeps_from_its_lowest_open_hold_and_a_dropped_hold_frees_it() {
    let holds = Holds::default();
    let early = holds.hold(starts(&[("p", 5), ("q", 9)]));
    let late = holds.hold(starts(&[("p", 7)]));
    assert_eq!(holds.floor("p"), Some(5));
    assert_eq!(holds.floor("q"), Some(9));
    drop(early);
    assert_eq!(holds.floor("p"), Some(7));
    assert_eq!(holds.floor("q"), None);
    drop(late);
    assert_eq!(holds.floor("p"), None);
}

#[test]
fn a_floor_reads_while_a_hold_is_being_taken() {
    let holds = Holds::default();
    let _held = holds.hold(starts(&[("p", 5)]));
    let taking = holds.sessions();
    assert_eq!(holds.floor("p"), Some(5));
    drop(taking);
}
