//! Observing open planes: a weak per-plane watch handle and the
//! process-wide [`PlaneMonitor`] that attributes every open plane to the
//! module that created it.
//!
//! Planes are per-use and subsystem-owned, so there
//! is deliberately no central object that OWNS them — the monitor only
//! watches. Each creator registers its plane right after bring-up with the
//! module name that opened it; a [`PlaneWatch`] holds a weak reference, so
//! registration never extends a plane's life, and a snapshot self-prunes
//! entries whose plane is gone.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::plane::{StatsSnapshot, TrafficSnapshot};

/// A point-in-time view of one open plane's accounting: drop/error stats and
/// successful-traffic counters together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneObservation {
    pub stats: StatsSnapshot,
    pub traffic: TrafficSnapshot,
}

/// A type-erased weak observer for one plane (see [`crate::DataPlane::watch`]).
/// `observe` yields `None` once the plane is gone — every handle dropped and
/// its pumps stopped — which is a monitor's cue to forget the entry.
#[derive(Clone)]
pub struct PlaneWatch(Arc<dyn Fn() -> Option<PlaneObservation> + Send + Sync>);

impl PlaneWatch {
    pub fn new(observe: impl Fn() -> Option<PlaneObservation> + Send + Sync + 'static) -> Self {
        PlaneWatch(Arc::new(observe))
    }

    pub fn observe(&self) -> Option<PlaneObservation> {
        (self.0)()
    }
}

impl std::fmt::Debug for PlaneWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneWatch").finish_non_exhaustive()
    }
}

/// One open plane in a [`PlaneMonitor::snapshot`]: who created it, which
/// service it carries, how long it has been open, and its live accounting.
#[derive(Clone, Copy, Debug)]
pub struct PlaneReport {
    /// The module that created the plane (registration-time attribution).
    pub owner: &'static str,
    /// The `service` metric label — a MACHINE contract a dashboard keys on,
    /// so the plane supplies it rather than deriving it from the lane id: an
    /// id now comes from the registry, and a label that moved with it would
    /// rename a metric every time a network renumbered a lane.
    pub service: &'static str,
    pub age: Duration,
    pub observation: PlaneObservation,
}

struct PlaneEntry {
    owner: &'static str,
    service: &'static str,
    opened_at: Instant,
    watch: PlaneWatch,
}

/// The registry of open planes. Cloneable handle; creators `register`,
/// observers `snapshot`. Holding it never keeps a plane alive.
#[derive(Clone, Default)]
pub struct PlaneMonitor {
    planes: Arc<Mutex<Vec<PlaneEntry>>>,
}

impl PlaneMonitor {
    /// Record an open plane under the module that created it. Call once per
    /// plane, right after bring-up.
    pub fn register(&self, owner: &'static str, service: &'static str, watch: PlaneWatch) {
        self.planes.lock().expect("planes lock").push(PlaneEntry {
            owner,
            service,
            opened_at: Instant::now(),
            watch,
        });
    }

    /// Observe every open plane; entries whose plane is gone are pruned.
    pub fn snapshot(&self) -> Vec<PlaneReport> {
        let mut planes = self.planes.lock().expect("planes lock");
        let mut reports = Vec::with_capacity(planes.len());
        planes.retain(|entry| match entry.watch.observe() {
            Some(observation) => {
                reports.push(PlaneReport {
                    owner: entry.owner,
                    service: entry.service,
                    age: entry.opened_at.elapsed(),
                    observation,
                });
                true
            }
            None => false,
        });
        reports
    }
}

impl std::fmt::Debug for PlaneMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let planes = self.planes.lock().expect("planes lock");
        f.debug_struct("PlaneMonitor")
            .field("planes", &planes.len())
            .finish()
    }
}
