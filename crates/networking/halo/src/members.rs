//! The current member set halo seats and dials: each member's key, the halo address it
//! registered on the chain if it has one, the HTTPS address it serves if any, and whether it votes.

use std::collections::HashMap;
use std::sync::RwLock;

use commonware_cryptography::ed25519::PublicKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seat {
    pub key: PublicKey,
    pub halo: Option<String>,
    pub https: Option<String>,
    pub validator: bool,
}

#[derive(Default)]
pub struct Members {
    seats: RwLock<HashMap<PublicKey, Seat>>,
}

impl Members {
    pub fn seat(&self, seats: impl IntoIterator<Item = Seat>) -> Vec<PublicKey> {
        let seated: HashMap<PublicKey, Seat> = seats
            .into_iter()
            .map(|seat| (seat.key.clone(), seat))
            .collect();
        let mut held = self
            .seats
            .write()
            .expect("the seats lock is never poisoned");
        let removed = held.keys().filter(|key| !seated.contains_key(*key));
        let moved = seated
            .iter()
            .filter(|(key, seat)| held.get(*key) != Some(*seat))
            .map(|(key, _)| key);
        let changed = removed.chain(moved).cloned().collect();
        *held = seated;
        changed
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<PublicKey, Seat>> {
        self.seats.read().expect("the seats lock is never poisoned")
    }

    pub fn seated(&self, key: &PublicKey) -> bool {
        self.read().contains_key(key)
    }

    pub fn validator(&self, key: &PublicKey) -> bool {
        self.read().get(key).is_some_and(|seat| seat.validator)
    }

    pub fn halo(&self, key: &PublicKey) -> Option<String> {
        self.read().get(key)?.halo.clone()
    }

    pub fn https(&self, key: &PublicKey) -> Option<String> {
        self.read().get(key)?.https.clone()
    }

    pub fn https_asked(&self, me: &PublicKey) -> Vec<String> {
        let seats = self.read();
        let mine = seats.get(me).and_then(|seat| seat.https.clone());
        let mut others: Vec<(&PublicKey, &String)> = seats
            .values()
            .filter(|seat| seat.key != *me)
            .filter_map(|seat| Some((&seat.key, seat.https.as_ref()?)))
            .collect();
        others.sort();
        mine.into_iter()
            .chain(others.into_iter().map(|(_, https)| https.clone()))
            .collect()
    }

    pub fn candidates(&self, me: &PublicKey, first: Option<&PublicKey>) -> Vec<PublicKey> {
        let seats = self.read();
        let mut validators: Vec<&PublicKey> = Vec::new();
        let mut residents: Vec<&PublicKey> = Vec::new();
        for seat in seats.values() {
            let pile = if seat.validator {
                &mut validators
            } else {
                &mut residents
            };
            pile.push(&seat.key);
        }
        validators.sort();
        residents.sort();
        let ordered = first.into_iter().chain(validators).chain(residents);
        let mut candidates: Vec<PublicKey> = Vec::new();
        for key in ordered {
            let askable = key != me && seats.contains_key(key);
            let fresh = !candidates.contains(key);
            if askable && fresh {
                candidates.push(key.clone());
            }
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::*;

    fn key(seed: u64) -> PublicKey {
        PrivateKey::from_seed(seed).public_key()
    }

    fn seat(seed: u64, validator: bool) -> Seat {
        Seat {
            key: key(seed),
            halo: Some(format!("localhost:{seed}")),
            https: None,
            validator,
        }
    }

    #[test]
    fn a_reseat_names_the_members_it_adds_removes_or_moves() {
        let members = Members::default();
        let mut first = members.seat([seat(0, true), seat(1, true), seat(2, false)]);
        first.sort();
        let mut all = vec![key(0), key(1), key(2)];
        all.sort();
        assert_eq!(first, all);
        let moved = Seat {
            halo: Some("elsewhere.example.org:9".into()),
            ..seat(1, true)
        };
        let mut changed = members.seat([seat(0, true), moved, seat(2, true), seat(3, false)]);
        changed.sort();
        let mut expected = vec![key(1), key(2), key(3)];
        expected.sort();
        assert_eq!(changed, expected);
        assert_eq!(
            members.halo(&key(1)).as_deref(),
            Some("elsewhere.example.org:9")
        );
        let removed = members.seat([seat(0, true)]);
        assert_eq!(removed.len(), 3);
        assert!(members.seat([seat(0, true)]).is_empty());
    }

    #[test]
    fn a_fetch_asks_the_sender_then_the_validators_then_the_residents_never_itself() {
        let members = Members::default();
        members.seat([seat(0, true), seat(1, true), seat(2, false), seat(3, false)]);
        let mut validators = vec![key(0), key(1)];
        validators.sort();
        let mut residents = vec![key(2), key(3)];
        residents.sort();
        let asked = members.candidates(&key(1), Some(&key(3)));
        let others: Vec<PublicKey> = validators
            .into_iter()
            .chain(residents)
            .filter(|other| *other != key(1) && *other != key(3))
            .collect();
        let expected: Vec<PublicKey> = std::iter::once(key(3)).chain(others).collect();
        assert_eq!(asked, expected);
        assert!(
            members
                .candidates(&key(0), Some(&key(9)))
                .iter()
                .all(|k| *k != key(9))
        );
    }

    #[test]
    fn a_member_asks_its_own_https_first_then_the_others_in_key_order() {
        let members = Members::default();
        let served = |seed: u64| Seat {
            https: Some(format!("wss://member-{seed}.example")),
            ..seat(seed, true)
        };
        members.seat([served(1), served(2), seat(3, false), served(4)]);
        let mut others = vec![(key(1), 1), (key(4), 4)];
        others.sort();
        let expected: Vec<String> = std::iter::once(2)
            .chain(others.into_iter().map(|(_, seed)| seed))
            .map(|seed| format!("wss://member-{seed}.example"))
            .collect();
        assert_eq!(members.https_asked(&key(2)), expected);
        let unserved: Vec<String> = members.https_asked(&key(3));
        assert_eq!(unserved.len(), 3);
    }

    #[test]
    fn a_member_seated_with_no_halo_address_is_admitted_and_names_none() {
        let members = Members::default();
        let unaddressed = Seat {
            halo: None,
            ..seat(1, true)
        };
        members.seat([seat(0, true), unaddressed]);
        assert!(members.seated(&key(1)));
        assert_eq!(members.halo(&key(1)), None);
        assert_eq!(members.halo(&key(0)).as_deref(), Some("localhost:0"));
    }
}
