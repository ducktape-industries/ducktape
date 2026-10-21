//! What the queries this node's clients ask burn: one `query_fuel` line as each epoch ends, the
//! measure `load.query_fuel` is set from.

use abi::ProgramId;
use tokio::sync::{mpsc, watch};

pub(crate) struct Queried {
    pub(crate) program: ProgramId,
    pub(crate) fuel: u64,
    pub(crate) ran_out: bool,
}

const WIDTHS: usize = u64::BITS as usize + 1;

struct Tally {
    epoch: u64,
    count: u64,
    ran_out: u64,
    max: u64,
    max_program: ProgramId,
    widths: [u64; WIDTHS],
}

impl Tally {
    fn new(epoch: u64) -> Tally {
        Tally {
            epoch,
            count: 0,
            ran_out: 0,
            max: 0,
            max_program: ProgramId::new(),
            widths: [0; WIDTHS],
        }
    }

    fn observe(&mut self, queried: Queried) {
        self.count += 1;
        self.ran_out += u64::from(queried.ran_out);
        self.widths[width(queried.fuel)] += 1;
        if queried.fuel >= self.max {
            self.max = queried.fuel;
            self.max_program = queried.program;
        }
    }

    fn at_most(&self, percent: u128) -> u64 {
        let wanted = (u128::from(self.count) * percent).div_ceil(100);
        let mut seen = 0;
        for (width, count) in self.widths.iter().enumerate() {
            seen += u128::from(*count);
            if seen >= wanted {
                return most_of(width);
            }
        }
        u64::MAX
    }

    fn log(&self, height: u64) {
        tracing::info!(
            target: "ducktape::query",
            event = "query_fuel",
            epoch = self.epoch,
            height,
            count = self.count,
            ran_out = self.ran_out,
            max = self.max,
            max_program = %self.max_program,
            p50_at_most = self.at_most(50),
            p99_at_most = self.at_most(99),
            "the fuel the queries this node answered burned over the epoch"
        );
    }
}

fn width(fuel: u64) -> usize {
    (u64::BITS - fuel.leading_zeros()) as usize
}

fn most_of(width: usize) -> u64 {
    1u64.checked_shl(width as u32)
        .map_or(u64::MAX, |past| past - 1)
}

enum Heard {
    Tip,
    Queried(Queried),
    Ended,
}

pub(crate) async fn census(
    epoch_length: u64,
    mut queries: mpsc::UnboundedReceiver<Queried>,
    mut tips: watch::Receiver<u64>,
) {
    let mut tally = Tally::new(*tips.borrow_and_update() / epoch_length);
    loop {
        let heard = tokio::select! {
            biased;
            changed = tips.changed() => match changed {
                Ok(()) => Heard::Tip,
                Err(_) => Heard::Ended,
            },
            Some(queried) = queries.recv() => Heard::Queried(queried),
        };
        match heard {
            Heard::Tip => tally = tipped(tally, epoch_length, &mut queries, &mut tips),
            Heard::Queried(queried) => tally.observe(queried),
            Heard::Ended => return,
        }
    }
}

fn tipped(
    mut tally: Tally,
    epoch_length: u64,
    queries: &mut mpsc::UnboundedReceiver<Queried>,
    tips: &mut watch::Receiver<u64>,
) -> Tally {
    let height = *tips.borrow_and_update();
    let epoch = height / epoch_length;
    if epoch == tally.epoch {
        return tally;
    }
    while let Ok(queried) = queries.try_recv() {
        tally.observe(queried);
    }
    tally.log(height);
    Tally::new(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queried(program: &str, fuel: u64, ran_out: bool) -> Queried {
        Queried {
            program: program.into(),
            fuel,
            ran_out,
        }
    }

    #[test]
    fn a_tally_names_the_most_fuel_and_its_program_and_the_power_of_two_at_each_percentile() {
        let mut tally = Tally::new(3);
        for fuel in 1..=100 {
            tally.observe(queried("chat", fuel * 1000, false));
        }
        tally.observe(queried("forge", 9_000_000, true));
        assert_eq!((tally.count, tally.ran_out), (101, 1));
        assert_eq!(
            (tally.max, tally.max_program.as_str()),
            (9_000_000, "forge")
        );
        assert_eq!(tally.at_most(50), 65_535);
        assert_eq!(tally.at_most(99), 131_071);
        assert_eq!(Tally::new(0).at_most(99), 0);
        let mut top = Tally::new(0);
        top.observe(queried("spin", u64::MAX, false));
        assert_eq!(top.at_most(50), u64::MAX);
    }

    #[test]
    fn a_tip_in_the_tallied_epoch_keeps_the_tally_and_one_past_it_starts_the_next() {
        let (sender, mut queries) = mpsc::unbounded_channel();
        let (tips, mut heard) = watch::channel(0);
        let mut tally = Tally::new(0);
        tally.observe(queried("chat", 5, false));
        tips.send(3).unwrap();
        let tally = tipped(tally, 4, &mut queries, &mut heard);
        assert_eq!((tally.epoch, tally.count), (0, 1));
        sender.send(queried("chat", 7, false)).unwrap();
        tips.send(4).unwrap();
        let tally = tipped(tally, 4, &mut queries, &mut heard);
        assert_eq!((tally.epoch, tally.count), (1, 0));
        assert!(queries.try_recv().is_err());
    }
}
