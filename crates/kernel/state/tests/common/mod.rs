//! What the store-open tests share: twelve programs, each block writing and deleting keys in all
//! of them, over one storage directory.
use std::collections::BTreeMap;
use std::path::Path;

use state::{Storage, Writes};

const PROGRAMS: u64 = 12;
pub const BLOCKS: u64 = 24;

pub fn programs() -> Vec<String> {
    (0..PROGRAMS)
        .map(|index| format!("program{index}"))
        .collect()
}

pub fn block(height: u64) -> Writes {
    let keys = |index: u64| {
        (0..8 * (index + 1))
            .map(|key| {
                let name = format!("{index}/{key}").into_bytes();
                let deleted = (key + height).is_multiple_of(7);
                let value = format!("{height}/{key}").into_bytes();
                (name, (!deleted).then_some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: programs()
            .into_iter()
            .zip(0..)
            .map(|(program, index)| (program, keys(index)))
            .collect(),
    }
}

pub fn storage(dir: &Path) -> Storage {
    Storage::open(&dir.join("state")).unwrap()
}
