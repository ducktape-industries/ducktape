//! The leader rotation every engine runs and `engine::proposer` reads blocks back with:
//! round-robin, started at a validator the previous epoch also seated.

use commonware_consensus::simplex::elector::{Config, Elector, Terms};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::{Participant, Round, TermLength};
use commonware_cryptography::certificate::Verifier;
use commonware_cryptography::ed25519::PublicKey;
use commonware_utils::ordered::Set;

#[derive(Clone, Debug)]
pub(crate) struct Continuing {
    epoch: u64,
    previous: Option<Set<PublicKey>>,
}

impl Continuing {
    pub(crate) fn new(epoch: u64, previous: Option<Set<PublicKey>>) -> Continuing {
        Continuing { epoch, previous }
    }

    fn continues(&self, key: &PublicKey) -> bool {
        self.previous
            .as_ref()
            .is_some_and(|previous| previous.position(key).is_some())
    }
}

impl Config<Scheme> for Continuing {
    type Elector = Rotation;

    fn build(self, participants: &Set<PublicKey>) -> Rotation {
        assert!(!participants.is_empty(), "no participants");
        let seats = participants.len() as u64;
        let round_robin = (self.epoch % seats + 1) % seats;
        let continuing = (0..seats)
            .map(|shift| (round_robin + shift) % seats)
            .find(|&index| {
                participants
                    .get(index as usize)
                    .is_some_and(|key| self.continues(key))
            });
        Rotation {
            seats,
            first: continuing.unwrap_or(round_robin),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Rotation {
    seats: u64,
    first: u64,
}

impl Elector<Scheme> for Rotation {
    fn terms(&self) -> Terms {
        Terms::rotating()
    }

    fn elect(
        &self,
        round: Round,
        _certificate: Option<&<Scheme as Verifier>::Certificate>,
    ) -> Participant {
        let term = round.view().term_index(TermLength::ONE);
        let since_first = term.saturating_sub(1) % self.seats;
        Participant::from_usize(((self.first + since_first) % self.seats) as usize)
    }
}

#[cfg(test)]
mod tests {
    use commonware_consensus::simplex::elector::RoundRobin;
    use commonware_consensus::types::{Epoch, View};
    use commonware_cryptography::{Sha256, Signer as _, ed25519};

    use super::*;

    fn keys(seeds: impl IntoIterator<Item = u64>) -> Set<PublicKey> {
        let keys: Vec<_> = seeds
            .into_iter()
            .map(|seed| ed25519::PrivateKey::from_seed(seed).public_key())
            .collect();
        Set::try_from(keys).unwrap()
    }

    fn round(epoch: u64, view: u64) -> Round {
        Round::new(Epoch::new(epoch), View::new(view))
    }

    fn leaders(elector: &impl Elector<Scheme>, epoch: u64, views: u64) -> Vec<u32> {
        (1..=views)
            .map(|view| elector.elect(round(epoch, view), None).get())
            .collect()
    }

    fn round_robin(epoch: u64, seats: &Set<PublicKey>, views: u64) -> Vec<u32> {
        let elector = Config::<Scheme>::build(RoundRobin::<Sha256>::default(), seats);
        leaders(&elector, epoch, views)
    }

    #[test]
    fn with_no_previous_set_it_is_round_robin() {
        let seats = keys(1..=4);
        for epoch in [0, 1, 5, u64::MAX] {
            let elector = Continuing::new(epoch, None).build(&seats);
            assert_eq!(leaders(&elector, epoch, 12), round_robin(epoch, &seats, 12));
        }
    }

    #[test]
    fn with_no_continuing_validator_it_is_round_robin() {
        let seats = keys(1..=4);
        let replaced = keys(5..=7);
        for epoch in [1, 2, 3, 4] {
            let elector = Continuing::new(epoch, Some(replaced.clone())).build(&seats);
            assert_eq!(leaders(&elector, epoch, 12), round_robin(epoch, &seats, 12));
        }
    }

    #[test]
    fn the_first_leader_is_a_validator_the_previous_epoch_seated() {
        let seats = keys(1..=4);
        for epoch in 1..=8 {
            for newcomer in seats.iter() {
                let previous: Vec<_> = seats
                    .iter()
                    .filter(|key| *key != newcomer)
                    .cloned()
                    .collect();
                let previous = Set::try_from(previous).unwrap();
                let elector = Continuing::new(epoch, Some(previous.clone())).build(&seats);
                let first = elector.elect(round(epoch, 1), None);
                let leader = seats.get(first.get() as usize).unwrap();
                assert!(previous.position(leader).is_some(), "epoch {epoch}");
            }
        }
    }

    #[test]
    fn when_round_robin_already_starts_at_a_continuing_validator_nothing_moves() {
        let seats = keys(1..=4);
        let epoch = 1;
        let first = round_robin(epoch, &seats, 1)[0] as usize;
        let previous: Vec<_> = seats
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != (first + 1) % seats.len())
            .map(|(_, key)| key.clone())
            .collect();
        let previous = Set::try_from(previous).unwrap();
        let elector = Continuing::new(epoch, Some(previous)).build(&seats);
        assert_eq!(leaders(&elector, epoch, 12), round_robin(epoch, &seats, 12));
    }

    #[test]
    fn a_newcomer_in_round_robins_first_seat_leads_last_in_the_first_rotation() {
        let seats = keys(1..=4);
        let epoch = 1;
        let newcomer = round_robin(epoch, &seats, 1)[0] as usize;
        let previous: Vec<_> = seats
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != newcomer)
            .map(|(_, key)| key.clone())
            .collect();
        let previous = Set::try_from(previous).unwrap();
        let elector = Continuing::new(epoch, Some(previous)).build(&seats);
        let rotated = leaders(&elector, epoch, 4);
        assert_eq!(rotated[0] as usize, (newcomer + 1) % seats.len());
        assert_eq!(rotated[3] as usize, newcomer);
    }

    #[test]
    fn every_validator_leads_once_in_each_run_of_as_many_views_as_seats() {
        let seats = keys(1..=5);
        let previous = keys([3, 9]);
        for epoch in 0..10 {
            let elector = Continuing::new(epoch, Some(previous.clone())).build(&seats);
            for start in [1, 4, 17] {
                let mut led: Vec<u32> = (start..start + 5)
                    .map(|view| elector.elect(round(epoch, view), None).get())
                    .collect();
                led.sort_unstable();
                assert_eq!(led, vec![0, 1, 2, 3, 4], "epoch {epoch} from view {start}");
            }
        }
    }
}
