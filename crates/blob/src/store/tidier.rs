// The thread a node's blob store tidies on: each wake asks for one more round of one segment's
// budget, wakes heard while a round runs fold into that one, and a drop lets the round in hand end.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::{Blobs, Result};

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
    pub fn spawn(blobs: Blobs) -> Result<Tidier> {
        let bell = Arc::new(Bell::default());
        let heard = bell.clone();
        let thread = std::thread::Builder::new()
            .name("blob-tidy".to_owned())
            .spawn(move || tidying(&blobs, &heard))?;
        Ok(Tidier {
            bell,
            thread: Some(thread),
        })
    }

    pub fn wake(&self) {
        self.bell.state().due = true;
        self.bell.rung.notify_all();
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

fn tidying(blobs: &Blobs, bell: &Bell) {
    loop {
        let mut state = bell.state();
        while !state.due && !state.stop {
            state = bell.wait(state);
        }
        if state.stop {
            return;
        }
        state.due = false;
        #[cfg(test)]
        {
            state.running = true;
            state.rounds += 1;
        }
        drop(state);
        if let Err(error) = blobs.tidy() {
            tracing::warn!(
                target: "ducktape::blobs",
                event = "blob_tidy_failed",
                %error,
                "the blob store kept a worn segment or an old file this round; the next wake tries again"
            );
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
        let tidier = Tidier::spawn(blobs.clone()).unwrap();

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
    }
}
