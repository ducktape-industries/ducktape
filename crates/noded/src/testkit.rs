//! Seams a test parks a running node at, compiled only with the `testkit` feature.

use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot};

#[derive(Default)]
pub struct Gate {
    armed: Mutex<Option<mpsc::UnboundedSender<Parked>>>,
}

pub struct Parked {
    pub height: u64,
    release: oneshot::Sender<()>,
}

impl Gate {
    pub fn arm(&self) -> mpsc::UnboundedReceiver<Parked> {
        let (parking, parked) = mpsc::unbounded_channel();
        *self.armed.lock().expect("the gate lock is never poisoned") = Some(parking);
        parked
    }

    pub(crate) async fn pass(&self, height: u64) {
        let armed = self
            .armed
            .lock()
            .expect("the gate lock is never poisoned")
            .clone();
        let Some(parking) = armed else {
            return;
        };
        let (release, released) = oneshot::channel();
        let told = parking.send(Parked { height, release });
        if told.is_err() {
            return;
        }
        let _ = released.await;
    }
}

impl Parked {
    pub fn release(self) {
        let _ = self.release.send(());
    }
}
