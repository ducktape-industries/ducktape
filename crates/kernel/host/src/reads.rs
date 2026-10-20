// What a standing query read, recorded by the host as it runs: each key it got, the range each
// scan covered and each program it read whole, so a block's writes tell whether its answer moved.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::{Mutex, PoisonError};

use abi::{Entry, ProgramId, Scan};

pub type Written = BTreeMap<ProgramId, BTreeSet<Vec<u8>>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadSet {
    points: BTreeMap<ProgramId, BTreeSet<Vec<u8>>>,
    spans: BTreeMap<ProgramId, Vec<Span>>,
    wholes: BTreeSet<ProgramId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    lo: Vec<u8>,
    hi: Option<Vec<u8>>,
}

impl ReadSet {
    pub fn point(&mut self, program: &str, key: &[u8]) {
        self.points
            .entry(program.to_owned())
            .or_default()
            .insert(key.to_vec());
    }

    pub fn scanned(&mut self, program: &str, scan: &Scan, entries: &[Entry]) {
        let full = scan
            .limit
            .is_some_and(|limit| entries.len() as u64 >= limit);
        let last = entries.last().filter(|_| full);
        let span = match (last, scan.reverse) {
            (None, _) => Span {
                lo: scan.lo.clone(),
                hi: scan.hi.clone(),
            },
            (Some(last), false) => Span {
                lo: scan.lo.clone(),
                hi: Some([last.key.as_slice(), &[0]].concat()),
            },
            (Some(last), true) => Span {
                lo: last.key.clone(),
                hi: scan.hi.clone(),
            },
        };
        self.spans.entry(program.to_owned()).or_default().push(span);
    }

    pub fn whole(&mut self, program: &str) {
        self.wholes.insert(program.to_owned());
    }

    pub fn union(&mut self, other: ReadSet) {
        for (program, keys) in other.points {
            self.points.entry(program).or_default().extend(keys);
        }
        for (program, spans) in other.spans {
            self.spans.entry(program).or_default().extend(spans);
        }
        self.wholes.extend(other.wholes);
    }

    pub fn touched(&self, written: &Written) -> bool {
        written
            .iter()
            .any(|(program, keys)| self.touches(program, keys))
    }

    fn touches(&self, program: &str, keys: &BTreeSet<Vec<u8>>) -> bool {
        let whole = self.wholes.contains(program);
        let pointed = self
            .points
            .get(program)
            .is_some_and(|points| meet(points, keys));
        let spanned = self
            .spans
            .get(program)
            .is_some_and(|spans| spans.iter().any(|span| span.holds_any(keys)));
        whole || pointed || spanned
    }
}

fn meet(points: &BTreeSet<Vec<u8>>, keys: &BTreeSet<Vec<u8>>) -> bool {
    let (fewer, more) = match points.len() <= keys.len() {
        true => (points, keys),
        false => (keys, points),
    };
    fewer.iter().any(|key| more.contains(key))
}

impl Span {
    fn holds_any(&self, keys: &BTreeSet<Vec<u8>>) -> bool {
        let empty = self.hi.as_ref().is_some_and(|hi| *hi <= self.lo);
        if empty {
            return false;
        }
        let hi = match &self.hi {
            Some(hi) => Bound::Excluded(hi.as_slice()),
            None => Bound::Unbounded,
        };
        let range = (Bound::Included(self.lo.as_slice()), hi);
        keys.range::<[u8], _>(range).next().is_some()
    }
}

#[derive(Default)]
pub struct Recorder(Mutex<ReadSet>);

impl Recorder {
    pub fn note(&self, note: impl FnOnce(&mut ReadSet)) {
        let mut reads = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        note(&mut reads);
    }

    pub fn into_reads(self) -> ReadSet {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}
