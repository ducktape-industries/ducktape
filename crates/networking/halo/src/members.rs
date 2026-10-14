//! The current member set halo admits and dials: each member's key, the halo address it
//! registered on the chain, and whether it votes.

use std::collections::HashMap;
use std::sync::RwLock;

use commonware_cryptography::ed25519::PublicKey;

#[derive(Clone, Debug)]
pub struct Seat {
    pub key: PublicKey,
    pub halo: String,
    pub validator: bool,
}

#[derive(Default)]
pub struct Members {
    seats: RwLock<HashMap<PublicKey, Seat>>,
}

impl Members {
    pub fn seat(&self, seats: impl IntoIterator<Item = Seat>) {
        let seated = seats
            .into_iter()
            .map(|seat| (seat.key.clone(), seat))
            .collect();
        *self
            .seats
            .write()
            .expect("the seats lock is never poisoned") = seated;
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<PublicKey, Seat>> {
        self.seats.read().expect("the seats lock is never poisoned")
    }

    pub fn admits(&self, key: &PublicKey) -> bool {
        self.read().contains_key(key)
    }

    pub fn validator(&self, key: &PublicKey) -> bool {
        self.read().get(key).is_some_and(|seat| seat.validator)
    }

    pub fn halo(&self, key: &PublicKey) -> Option<String> {
        Some(self.read().get(key)?.halo.clone())
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
            halo: format!("localhost:{seed}"),
            validator,
        }
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
}
