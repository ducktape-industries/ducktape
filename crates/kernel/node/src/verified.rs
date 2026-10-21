// The frames a node checked when it took them, by their exact bytes, so a block applies without
// checking them again; the frames it never took are checked across the threads the writer may use.

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use abi::Refusal;
use dashmap::DashSet;
use host::Submission;

use crate::frame::Checked;

const THREAD: &str = "verify";

pub(crate) type Check<'a> = dyn Fn(&[u8]) -> Result<Submission, Refusal> + Sync + 'a;

#[derive(Clone)]
pub struct Verified {
    network: Arc<[u8]>,
    frames: Arc<DashSet<Entry>>,
}

struct Entry(Checked);

impl Borrow<[u8]> for Entry {
    fn borrow(&self) -> &[u8] {
        self.0.frame()
    }
}

impl Hash for Entry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.frame().hash(state);
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Entry) -> bool {
        self.0.frame() == other.0.frame()
    }
}

impl Eq for Entry {}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verified")
            .field("network", &self.network)
            .field("frames", &self.frames.len())
            .finish()
    }
}

impl Verified {
    pub fn new(network: &[u8]) -> Verified {
        Verified {
            network: Arc::from(network),
            frames: Arc::new(DashSet::new()),
        }
    }

    pub fn network(&self) -> &[u8] {
        &self.network
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(crate) fn insert(&self, checked: &Checked) -> bool {
        let same_network = checked.network() == self.network.as_ref();
        same_network && self.frames.insert(Entry(checked.clone()))
    }

    pub(crate) fn remove(&self, frame: &[u8]) {
        self.frames.remove(frame);
    }

    fn get(&self, frame: &[u8]) -> Option<Submission> {
        let entry = self.frames.get(frame)?;
        Some(entry.key().0.submission().clone())
    }
}

pub(crate) fn carried(
    frames: &[Vec<u8>],
    network: &[u8],
    verified: &Verified,
    check: &Check<'_>,
    threads: usize,
) -> Vec<(u32, Submission)> {
    let known = verified.network() == network;
    let mut found: Vec<Option<Submission>> = Vec::with_capacity(frames.len());
    let mut missed = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let hit = if known { verified.get(frame) } else { None };
        if hit.is_none() {
            missed.push(index);
        }
        found.push(hit);
    }
    for (index, submission) in checked(frames, &missed, check, threads) {
        found[index] = submission;
    }
    found
        .into_iter()
        .enumerate()
        .filter_map(|(index, submission)| Some((index as u32, submission?)))
        .collect()
}

fn checked(
    frames: &[Vec<u8>],
    missed: &[usize],
    check: &Check<'_>,
    threads: usize,
) -> Vec<(usize, Option<Submission>)> {
    let each = |part: &[usize]| -> Vec<(usize, Option<Submission>)> {
        part.iter()
            .map(|&index| (index, check(&frames[index]).ok()))
            .collect()
    };
    let threads = threads.min(missed.len());
    if threads <= 1 {
        return each(missed);
    }
    let share = missed.len().div_ceil(threads);
    let each = &each;
    std::thread::scope(|scope| {
        let running: Vec<_> = missed
            .chunks(share)
            .map(|part| {
                let spawned = std::thread::Builder::new()
                    .name(THREAD.into())
                    .spawn_scoped(scope, move || each(part));
                (part, spawned)
            })
            .collect();
        running
            .into_iter()
            .flat_map(|(part, spawned)| match spawned {
                Ok(handle) => match handle.join() {
                    Ok(done) => done,
                    Err(panic) => std::panic::resume_unwind(panic),
                },
                Err(_) => each(part),
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use commonware_cryptography::{Signer as _, ed25519};
    use host::{Submission, Tip};
    use keyscheme::KeyScheme;
    use keyscheme::testkit::{eth_key, eth_proof, eth_pubkey, ssh_key, ssh_proof, ssh_pubkey};

    use super::{Verified, carried};
    use crate::frame::{Body, Frame, NAMESPACE, verify};
    use crate::pool::Pool;

    const NETWORK: &[u8] = b"net";
    const ANCHOR: Tip = Tip {
        height: 0,
        id: [0; 32],
    };

    fn body(scheme: KeyScheme, signer: Vec<u8>, payload: &[u8]) -> Body {
        Body {
            scheme,
            signer,
            network: NETWORK.to_vec(),
            anchor: ANCHOR,
            salt: None,
            target: "probe".into(),
            payload: payload.to_vec(),
            client: None,
        }
    }

    fn ed25519_frame(seed: u64, payload: &[u8]) -> Vec<u8> {
        let key = ed25519::PrivateKey::from_seed(seed);
        Frame::sign(&key, NETWORK, ANCHOR, None, "probe", payload.to_vec()).encode()
    }

    fn eth_frame(seed: u8, payload: &[u8]) -> Vec<u8> {
        let key = eth_key(seed);
        let body = body(KeyScheme::Secp256k1, eth_pubkey(&key), payload);
        let proof = eth_proof(&key, NAMESPACE, &body.preimage());
        Frame { body, proof }.encode()
    }

    fn ssh_frame(seed: u8, payload: &[u8]) -> Vec<u8> {
        let key = ssh_key(seed);
        let body = body(KeyScheme::Ed25519, ssh_pubkey(&key), payload);
        let proof = ssh_proof(&key, NAMESPACE, &body.preimage());
        Frame { body, proof }.encode()
    }

    fn forged(frame: &[u8]) -> Vec<u8> {
        let mut forged = Frame::decode(frame).unwrap();
        let other = ed25519::PrivateKey::from_seed(99);
        forged.proof = other
            .sign(NAMESPACE, &forged.body.preimage())
            .as_ref()
            .to_vec();
        forged.encode()
    }

    fn serial(frames: &[Vec<u8>]) -> Vec<(u32, Submission)> {
        frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| Some((index as u32, verify(frame, NETWORK).ok()?)))
            .collect()
    }

    fn admitted(pool: &mut Pool, frame: &[u8]) {
        let checked = crate::admissible(frame.to_vec(), NETWORK).unwrap();
        pool.admit(checked, None, None);
    }

    fn counted(
        frames: &[Vec<u8>],
        network: &[u8],
        verified: &Verified,
        threads: usize,
    ) -> (Vec<(u32, Submission)>, usize) {
        let checks = AtomicUsize::new(0);
        let check = |frame: &[u8]| {
            checks.fetch_add(1, Ordering::SeqCst);
            verify(frame, network)
        };
        let carried = carried(frames, network, verified, &check, threads);
        (carried, checks.load(Ordering::SeqCst))
    }

    #[test]
    fn a_block_of_admitted_frames_applies_with_no_check() {
        let verified = Verified::new(NETWORK);
        let mut pool = Pool::new(verified.clone());
        let frames = [
            ed25519_frame(1, b"a"),
            eth_frame(2, b"b"),
            ssh_frame(3, b"c"),
        ];
        for frame in &frames {
            admitted(&mut pool, frame);
        }
        let (carried, checks) = counted(&frames, NETWORK, &verified, 2);
        assert_eq!(carried.len(), 3);
        assert_eq!(carried, serial(&frames));
        assert_eq!(checks, 0);
    }

    #[test]
    fn frames_never_admitted_are_each_checked_once_and_a_bad_one_is_dropped_in_place() {
        let verified = Verified::new(NETWORK);
        let mut pool = Pool::new(verified.clone());
        let taken = ed25519_frame(1, b"a");
        admitted(&mut pool, &taken);
        let frames = [
            ed25519_frame(2, b"b"),
            taken,
            eth_frame(3, b"c"),
            forged(&ed25519_frame(4, b"d")),
            ssh_frame(5, b"e"),
            b"not a frame".to_vec(),
            ed25519_frame(6, b"f"),
        ];
        let (carried, checks) = counted(&frames, NETWORK, &verified, 2);
        assert_eq!(carried, serial(&frames));
        let indices: Vec<u32> = carried.iter().map(|(index, _)| *index).collect();
        assert_eq!(indices, [0, 1, 2, 4, 6]);
        assert_eq!(checks, frames.len() - 1);
    }

    #[test]
    fn a_forged_proof_under_an_admitted_id_is_dropped() {
        let verified = Verified::new(NETWORK);
        let mut pool = Pool::new(verified.clone());
        let real = ed25519_frame(1, b"a");
        admitted(&mut pool, &real);
        let forged = forged(&real);
        let forged_id = Frame::decode(&forged).unwrap().body.submission().id();
        assert_eq!(verify(&real, NETWORK).unwrap().id(), forged_id);
        let (carried, checks) = counted(&[forged], NETWORK, &verified, 2);
        assert!(carried.is_empty());
        assert_eq!(checks, 1);
    }

    #[test]
    fn every_mix_of_admitted_and_unadmitted_frames_carries_what_the_serial_check_does() {
        let frames = [
            ed25519_frame(1, b"a"),
            eth_frame(2, b"b"),
            forged(&ed25519_frame(3, b"c")),
            ssh_frame(4, b"d"),
            ed25519_frame(5, b"e"),
        ];
        let admissible: Vec<usize> = (0..frames.len())
            .filter(|&index| verify(&frames[index], NETWORK).is_ok())
            .collect();
        let expected = serial(&frames);
        for mix in 0..1u32 << admissible.len() {
            let verified = Verified::new(NETWORK);
            let mut pool = Pool::new(verified.clone());
            let chosen: Vec<usize> = admissible
                .iter()
                .enumerate()
                .filter(|(bit, _)| mix & (1 << bit) != 0)
                .map(|(_, &index)| index)
                .collect();
            for &index in &chosen {
                admitted(&mut pool, &frames[index]);
            }
            for threads in 1..=3 {
                let (carried, checks) = counted(&frames, NETWORK, &verified, threads);
                assert_eq!(carried, expected, "mix {mix:b}, {threads} threads");
                let unadmitted = frames.len() - chosen.len();
                assert_eq!(checks, unadmitted, "mix {mix:b}, {threads} threads");
            }
        }
    }

    #[test]
    fn a_map_for_another_network_gives_no_hit() {
        let verified = Verified::new(NETWORK);
        let mut pool = Pool::new(verified.clone());
        let frame = ed25519_frame(1, b"a");
        admitted(&mut pool, &frame);
        let (carried, checks) = counted(&[frame], b"another network", &verified, 2);
        assert!(carried.is_empty());
        assert_eq!(checks, 1);
    }

    #[test]
    fn a_frame_pruned_from_the_pool_is_checked_again() {
        let verified = Verified::new(NETWORK);
        let mut pool = Pool::new(verified.clone());
        let frame = ed25519_frame(1, b"a");
        admitted(&mut pool, &frame);
        let id = verify(&frame, NETWORK).unwrap().id();
        let frames = [frame];
        assert_eq!(counted(&frames, NETWORK, &verified, 2).1, 0);
        pool.remove(&[id]);
        let (carried, checks) = counted(&frames, NETWORK, &verified, 2);
        assert_eq!(carried, serial(&frames));
        assert_eq!(checks, 1);
    }
}
