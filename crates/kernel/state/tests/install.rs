// A join's install of a state larger than the store takes in one write: fluent31 refuses a
// batch whose estimate reaches its WAL record limit, so the install writes it in pieces.

use std::collections::BTreeMap;

use state::{Storage, Writes};

const VALUE: usize = 1 << 20;

#[test]
#[ignore = "writes past 1 GiB into the temp dir: cargo test -p state --test install -- --include-ignored"]
fn an_install_whose_estimate_passes_the_store_record_limit_lands_whole() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(dir.path()).unwrap();
    let values = (1usize << 30) / VALUE + 1;
    let keys: BTreeMap<Vec<u8>, Option<Vec<u8>>> = (0..values)
        .map(|n| {
            let key = (n as u64).to_be_bytes().to_vec();
            let value = vec![(n % 251) as u8; VALUE];
            (key, Some(value))
        })
        .collect();
    let mut writes = Writes::default();
    writes.programs.insert("big".into(), keys);
    storage.install(9, &writes).unwrap();
    assert_eq!(storage.height().unwrap(), Some(9));
    for n in [0, values / 2, values - 1] {
        let key = (n as u64).to_be_bytes();
        let value = storage.get("big", &key).unwrap().unwrap();
        assert_eq!(value, vec![(n % 251) as u8; VALUE]);
    }
}

fn one(program: &str, key: &[u8], value: Vec<u8>) -> Writes {
    let mut writes = Writes::default();
    let keys = BTreeMap::from([(key.to_vec(), Some(value))]);
    writes.programs.insert(program.into(), keys);
    writes
}

#[test]
fn an_install_cut_part_way_leaves_no_height_and_the_next_install_leaves_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(dir.path()).unwrap();
    storage
        .install(5, &one("old", b"a", b"before".to_vec()))
        .unwrap();
    let mut cut = one("cut", b"b", b"small".to_vec());
    let whole_record = vec![7u8; 1 << 30];
    let keys = cut.programs.get_mut("cut").unwrap();
    keys.insert(b"c".to_vec(), Some(whole_record));
    let refused = storage.install(9, &cut);
    assert!(
        refused.is_err(),
        "a single value past the record limit is refused"
    );
    drop(storage);
    let storage = Storage::open(dir.path()).unwrap();
    assert_eq!(
        storage.height().unwrap(),
        None,
        "an install cut part way reads as no state"
    );
    storage
        .install(7, &one("new", b"d", b"after".to_vec()))
        .unwrap();
    assert_eq!(storage.height().unwrap(), Some(7));
    assert_eq!(storage.get("new", b"d").unwrap(), Some(b"after".to_vec()));
    assert_eq!(storage.get("old", b"a").unwrap(), None);
    assert_eq!(storage.get("cut", b"b").unwrap(), None);
}
