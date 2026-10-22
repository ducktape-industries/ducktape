// The thread a node's blob store tidies on: each wake asks for one more round of one segment's
// budget, wakes heard while a round runs fold into that one, a placement runs on the thread before
// its next round while its caller waits for it, and a drop lets the round in hand end.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{Blobs, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tidied {
    pub copied: u64,
    pub moved: u64,
    pub retired: u64,
}

pub struct Tidier {
    bell: Arc<Bell>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Bell {
    state: Mutex<Rung>,
    rung: Condvar,
}

#[derive(Default)]
struct Rung {
    due: bool,
    stop: bool,
    placing: Option<Box<dyn FnOnce() + Send>>,
    placed: u64,
    #[cfg(test)]
    running: bool,
    #[cfg(test)]
    rounds: u64,
}

impl Bell {
    fn state(&self) -> MutexGuard<'_, Rung> {
        self.state.lock().expect("the tidy bell is never poisoned")
    }

    fn wait<'a>(&self, state: MutexGuard<'a, Rung>) -> MutexGuard<'a, Rung> {
        self.rung
            .wait(state)
            .expect("the tidy bell is never poisoned")
    }
}

impl Tidier {
    pub fn spawn(
        blobs: Blobs,
        mut said: impl FnMut(&Tidied, Duration) + Send + 'static,
    ) -> Result<Tidier> {
        let bell = Arc::new(Bell::default());
        let heard = bell.clone();
        let thread = std::thread::Builder::new()
            .name("blob-tidy".to_owned())
            .spawn(move || tidying(&blobs, &heard, &mut said))?;
        Ok(Tidier {
            bell,
            thread: Some(thread),
        })
    }

    pub fn wake(&self) {
        self.bell.state().due = true;
        self.bell.rung.notify_all();
    }

    pub fn place(&self, step_aside: impl FnOnce() + Send + 'static) {
        let mut state = self.bell.state();
        state.placing = Some(Box::new(step_aside));
        let wanted = state.placed + 1;
        self.bell.rung.notify_all();
        loop {
            let ended = state.placed >= wanted || state.stop;
            if ended {
                return;
            }
            state = self.bell.wait(state);
        }
    }

    #[cfg(test)]
    fn settled(&self) -> u64 {
        let mut state = self.bell.state();
        while state.due || state.running {
            state = self.bell.wait(state);
        }
        state.rounds
    }
}

impl Drop for Tidier {
    fn drop(&mut self) {
        self.bell.state().stop = true;
        self.bell.rung.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn tidying(blobs: &Blobs, bell: &Bell, said: &mut impl FnMut(&Tidied, Duration)) {
    loop {
        let mut state = bell.state();
        while !state.due && !state.stop && state.placing.is_none() {
            state = bell.wait(state);
        }
        if state.stop {
            return;
        }
        if let Some(step_aside) = state.placing.take() {
            drop(state);
            step_aside();
            bell.state().placed += 1;
            bell.rung.notify_all();
            continue;
        }
        state.due = false;
        #[cfg(test)]
        {
            state.running = true;
            state.rounds += 1;
        }
        drop(state);
        let began = Instant::now();
        let round = blobs.tidy();
        let spent = began.elapsed();
        match &round {
            Ok(tidied) => said(tidied, spent),
            Err(error) => tracing::warn!(
                target: "ducktape::blobs",
                event = "blob_tidy_failed",
                %error,
                "the blob store kept a worn segment or an old file this round; the next wake tries again"
            ),
        }
        #[cfg(test)]
        {
            bell.state().running = false;
            bell.rung.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use abi::HashKind;

    use super::*;
    use crate::store::Sizes;
    use crate::{frame, id_of, name_of};

    #[test]
    fn wakes_heard_while_a_round_runs_fold_into_one_more_round() {
        let dir = tempfile::tempdir().unwrap();
        let sizes = Sizes {
            segment: 4096,
            frame: 512,
        };
        for i in 0..20u64 {
            let body: Vec<u8> = (0..1000u64).map(|j| (i * 31 + j * 7) as u8).collect();
            let framed = frame("part", &body).unwrap();
            let id = id_of(HashKind::Sha256, &framed).unwrap();
            fs::write(dir.path().join(name_of(&id)), &framed).unwrap();
        }
        let blobs = Blobs::sized(dir.path(), sizes).unwrap();
        let rounds = Arc::new(Mutex::new(Vec::new()));
        let heard = rounds.clone();
        let tidier = Tidier::spawn(blobs.clone(), move |tidied, _| {
            heard.lock().unwrap().push(*tidied);
        })
        .unwrap();

        let held = blobs.gate_copies();
        tidier.wake();
        held.reached();
        for _ in 0..5 {
            tidier.wake();
        }
        let next = blobs.gate_copies();
        drop(held);
        next.reached();
        drop(next);
        assert_eq!(
            tidier.settled(),
            2,
            "five wakes heard while a round ran ask for one more round"
        );
        assert_eq!(
            rounds.lock().unwrap().len(),
            2,
            "each round says what it did"
        );
    }

    #[test]
    fn a_placement_has_run_on_the_tidy_thread_once_place_returns() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let (told, heard) = std::sync::mpsc::channel();
        let rounds = told.clone();
        let tidier = Tidier::spawn(blobs, move |_, _| {
            let _ = rounds.send("a round".to_owned());
        })
        .unwrap();
        tidier.place(move || {
            let thread = std::thread::current();
            let _ = told.send(thread.name().unwrap_or_default().to_owned());
        });
        assert_eq!(heard.try_recv().unwrap(), "blob-tidy");
        tidier.wake();
        assert_eq!(heard.recv().unwrap(), "a round");
    }
}
