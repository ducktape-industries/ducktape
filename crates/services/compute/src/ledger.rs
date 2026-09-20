//! ledger.rs — the host-local admission ledger: announced capacity minus the
//! demands of currently RUNNING jobs. deliberately process-local (consensus
//! never sees load); a crashed node's over-commitments die with its leases.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use provider_host::RunCancellation;
use tokio::sync::Notify;

/// how many subsequent `gate()` calls a BOUNDED pending claim (its
/// announcement carried a saga deadline) may survive with no follow-up
/// event before this node gives up on it. Backstop only: the saga assigning
/// the lease elsewhere, or this node's own gate refusing it, both release
/// the claim immediately by name — this budget exists solely for the path
/// with no observable event at all (the saga is cancelled, or times out,
/// while this node's Accept never lands and `assignee` never leaves `None`;
/// the saga's own `cancel_attempt`/`Crank` deliberately emit nothing for an
/// unassigned attempt, so no event ever reaches this node to release it).
///
/// ponytail: a call-count beat, not the saga's real height/view-denominated
/// deadline — this crate has no way to read consensus height, and comparing
/// wall-clock time against a HEIGHT number would be flatly wrong (the
/// validator default `ConsensusTimePolicy::HeightIsTime` makes `deadline` a
/// raw block count, not a timestamp). Upgrade path: thread the current
/// height into `Worker::run` and compare directly.
pub(crate) const PENDING_CLAIM_BEAT_BUDGET: u64 = 64;

struct PendingClaim {
    reservation: ReservationGuard,
    /// whether the announcement carried a saga deadline at all — a claim
    /// with none is never locally swept either, mirroring the saga's own
    /// "no deadline, no lease: never expires" rule.
    bounded: bool,
    /// the ledger's beat counter at claim time; swept once the beat has
    /// advanced past it by [`PENDING_CLAIM_BEAT_BUDGET`].
    claimed_at_beat: u64,
}

pub struct ResourceLedger {
    capacity: BTreeMap<String, u64>,
    running: Arc<Mutex<BTreeMap<String, BTreeMap<String, u64>>>>,
    available: Arc<Notify>,
    /// PENDING claims: an announcement's Accept reserves here before the
    /// saga has assigned anything, so a second announcement in the same
    /// window sees the capacity as spoken-for (counted by `fits`/
    /// `within_capacity` via the same `running` table `try_reserve` writes
    /// to). Keyed by `"{saga_id}:{attempt}"`, same as a running reservation,
    /// so the winning own-lease request converts this exact guard instead
    /// of reserving twice.
    pending: Arc<Mutex<HashMap<String, PendingClaim>>>,
    /// incremented once per `gate()` call — the pool's own event-processing
    /// cadence, used as [`Self::tick_and_sweep_pending`]'s beat. Not a timer:
    /// nothing here ever sleeps or spawns to advance it.
    beat: Mutex<u64>,
}

const DEFAULT_VM_CORES: u64 = 4;
const DEFAULT_VM_MEM_GB: u64 = 8;

impl ResourceLedger {
    pub fn new(capacity: BTreeMap<String, u64>) -> Self {
        Self {
            capacity,
            running: Arc::new(Mutex::new(BTreeMap::new())),
            available: Arc::new(Notify::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            beat: Mutex::new(0),
        }
    }

    /// Turn omitted dimensions into an honest upper bound. On a sandboxed
    /// node, leaving (say) memory unspecified means the run may use the whole
    /// node, not zero memory: reserve and enforce the full announced capacity
    /// for that dimension. An empty ledger is the Direct path, where a
    /// demandless run remains deliberately unrestricted.
    pub(crate) fn accounted_demands(
        &self,
        demands: &BTreeMap<String, u64>,
    ) -> BTreeMap<String, u64> {
        if self.capacity.is_empty() {
            return demands.clone();
        }
        let mut accounted = demands.clone();
        for (dimension, capacity) in &self.capacity {
            accounted.entry(dimension.clone()).or_insert(*capacity);
        }
        accounted
    }

    /// Choose the VM's machine size from the original explicit demands. The
    /// ledger deliberately accounts omitted dimensions at their full announced
    /// capacity, but a demandless run still gets a small useful VM. Explicit
    /// dimensions remain unchanged; defaults are clamped to the capacity so a
    /// provider can never ask the hypervisor for more than this node announced.
    pub(crate) fn execution_limits(
        &self,
        demands: &BTreeMap<String, u64>,
    ) -> BTreeMap<String, u64> {
        if self.capacity.is_empty() {
            return demands.clone();
        }
        let mut limits = demands.clone();
        if let Some(capacity) = self.capacity.get("cores") {
            limits
                .entry("cores".into())
                .or_insert(DEFAULT_VM_CORES.min(*capacity));
        }
        if let Some(capacity) = self.capacity.get("mem_gb") {
            limits
                .entry("mem_gb".into())
                .or_insert(DEFAULT_VM_MEM_GB.min(*capacity));
        }
        limits
    }

    /// Whether this node could ever satisfy the demands. Unlike [`Self::fits`],
    /// this ignores current occupancy so an assigned attempt can queue behind
    /// work already running here instead of losing its effect.
    pub(crate) fn within_capacity(&self, demands: &BTreeMap<String, u64>) -> bool {
        demands
            .iter()
            .all(|(dim, want)| self.capacity.get(dim).is_some_and(|cap| cap >= want))
    }

    /// free = capacity − Σ running, per dimension; a demanded dimension the
    /// capacity never named is a mismatch (absent ≠ infinite). Callers pass
    /// [`Self::accounted_demands`] so omitted sandbox dimensions cost their
    /// full capacity; only the empty-capacity Direct path stays free.
    pub fn fits(&self, demands: &BTreeMap<String, u64>) -> bool {
        let running = self.running.lock().expect("ledger lock");
        self.fits_locked(&running, demands)
    }

    fn fits_locked(
        &self,
        running: &BTreeMap<String, BTreeMap<String, u64>>,
        demands: &BTreeMap<String, u64>,
    ) -> bool {
        demands.iter().all(|(dim, want)| {
            let Some(cap) = self.capacity.get(dim) else {
                return false;
            };
            let used: u64 = running.values().filter_map(|d| d.get(dim)).sum();
            cap.saturating_sub(used) >= *want
        })
    }

    /// atomically check and reserve one attempt's demands. `gate()` performs
    /// the cheap optimistic check, but concurrent offers can arrive before a
    /// spawned task starts; this second check is the admission linearizer.
    /// the guard releases on drop.
    pub fn try_reserve(
        &self,
        key: &str,
        demands: &BTreeMap<String, u64>,
    ) -> Option<ReservationGuard> {
        let mut running = self.running.lock().expect("ledger lock");
        if !self.fits_locked(&running, demands) {
            return None;
        }
        if !demands.is_empty() {
            running.insert(key.to_string(), demands.clone());
        }
        Some(ReservationGuard {
            running: Arc::clone(&self.running),
            available: Arc::clone(&self.available),
            key: key.to_string(),
        })
    }

    /// Take a PENDING reservation for an announcement's Accept claim: same
    /// atomic check-and-reserve as [`Self::try_reserve`], tracked separately
    /// so [`Self::take_pending`] can hand the exact guard to the winning
    /// own-lease request instead of reserving the same work twice. `bounded`
    /// says whether the announcement carried a saga deadline — see
    /// [`PENDING_CLAIM_BEAT_BUDGET`].
    pub(crate) fn claim_pending(
        &self,
        key: String,
        demands: &BTreeMap<String, u64>,
        bounded: bool,
    ) -> bool {
        let Some(reservation) = self.try_reserve(&key, demands) else {
            return false;
        };
        let claimed_at_beat = *self.beat.lock().expect("ledger lock");
        self.pending.lock().expect("ledger lock").insert(
            key,
            PendingClaim {
                reservation,
                bounded,
                claimed_at_beat,
            },
        );
        true
    }

    /// Remove and return the pending claim for `key`, if this node holds
    /// one — the winning own-lease request converts it into the running
    /// reservation; every other caller drops the returned guard immediately,
    /// releasing the claim.
    pub(crate) fn take_pending(&self, key: &str) -> Option<ReservationGuard> {
        self.pending
            .lock()
            .expect("ledger lock")
            .remove(key)
            .map(|claim| claim.reservation)
    }

    /// Release a pending claim this node will never run: the saga assigned
    /// the lease elsewhere, or this node's own follow-up gate refused it.
    pub(crate) fn release_pending(&self, key: &str) {
        drop(self.take_pending(key));
    }

    /// Advance the beat by one and sweep any BOUNDED pending claim whose
    /// budget has elapsed — the backstop for a saga that dies with
    /// `assignee` still `None` (cancelled or timed out before any Accept
    /// landed), which emits no event this node could otherwise release on.
    /// Called once per `gate()` invocation: the pool's own event cadence
    /// IS the beat, so this never sleeps or spawns anything.
    pub(crate) fn tick_and_sweep_pending(&self) {
        let beat = {
            let mut beat = self.beat.lock().expect("ledger lock");
            *beat += 1;
            *beat
        };
        self.pending
            .lock()
            .expect("ledger lock")
            .retain(|_, claim| {
                !claim.bounded
                    || beat.saturating_sub(claim.claimed_at_beat) < PENDING_CLAIM_BEAT_BUDGET
            });
    }

    /// Wait until the demands fit, then reserve them atomically. Register the
    /// waiter before each optimistic reserve so a release between the failed
    /// check and the await cannot be lost.
    pub(crate) async fn reserve_when_available(
        &self,
        key: &str,
        demands: &BTreeMap<String, u64>,
        cancellation: &RunCancellation,
    ) -> Option<ReservationGuard> {
        loop {
            if cancellation.is_cancelled() {
                return None;
            }
            let released = self.available.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Some(reservation) = self.try_reserve(key, demands) {
                return Some(reservation);
            }
            tokio::select! {
                _ = &mut released => {}
                _ = cancellation.cancelled() => return None,
            }
        }
    }
}

pub struct ReservationGuard {
    running: Arc<Mutex<BTreeMap<String, BTreeMap<String, u64>>>>,
    available: Arc<Notify>,
    key: String,
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        self.running.lock().expect("ledger lock").remove(&self.key);
        self.available.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn res(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn fits_is_per_dimension_and_absent_is_not_infinite() {
        let l = ResourceLedger::new(res(&[("cores", 8), ("mem_gb", 16)]));
        assert!(l.fits(&res(&[("cores", 8)])));
        assert!(!l.fits(&res(&[("cores", 9)])));
        assert!(!l.fits(&res(&[("gpu", 1)])), "absent dimension never fits");
        assert!(l.fits(&res(&[])), "demandless always fits");
        // empty capacity (direct node): only demandless fits.
        let bare = ResourceLedger::new(Default::default());
        assert!(bare.fits(&res(&[])));
        assert!(!bare.fits(&res(&[("cores", 1)])));
        assert!(l.within_capacity(&res(&[("cores", 8)])));
        assert!(!l.within_capacity(&res(&[("cores", 9)])));
    }

    #[test]
    fn omitted_sandbox_dimensions_account_for_full_capacity() {
        let l = ResourceLedger::new(res(&[("cores", 8), ("mem_gb", 16)]));
        assert_eq!(
            l.accounted_demands(&res(&[])),
            res(&[("cores", 8), ("mem_gb", 16)])
        );
        assert_eq!(
            l.accounted_demands(&res(&[("cores", 2)])),
            res(&[("cores", 2), ("mem_gb", 16)])
        );
        let direct = ResourceLedger::new(BTreeMap::new());
        assert!(direct.accounted_demands(&res(&[])).is_empty());
    }

    #[test]
    fn execution_limits_keep_explicit_demands_and_bound_defaults() {
        let l = ResourceLedger::new(res(&[("cores", 24), ("mem_gb", 94)]));
        assert_eq!(
            l.execution_limits(&res(&[])),
            res(&[("cores", 4), ("mem_gb", 8)])
        );
        assert_eq!(
            l.execution_limits(&res(&[("cores", 2)])),
            res(&[("cores", 2), ("mem_gb", 8)])
        );
        assert_eq!(
            l.execution_limits(&res(&[("cores", 2), ("mem_gb", 3)])),
            res(&[("cores", 2), ("mem_gb", 3)])
        );

        let small = ResourceLedger::new(res(&[("cores", 2), ("mem_gb", 4)]));
        assert_eq!(
            small.execution_limits(&res(&[])),
            res(&[("cores", 2), ("mem_gb", 4)])
        );
        let direct = ResourceLedger::new(BTreeMap::new());
        assert!(direct.execution_limits(&res(&[])).is_empty());
    }

    #[test]
    fn reservations_subtract_and_release_on_drop() {
        let l = ResourceLedger::new(res(&[("cores", 8)]));
        let guard = l
            .try_reserve("s1:0", &res(&[("cores", 6)]))
            .expect("first reservation fits");
        assert!(!l.fits(&res(&[("cores", 4)])), "6 of 8 reserved");
        assert!(l.fits(&res(&[("cores", 2)])));
        assert!(
            l.try_reserve("s2:0", &res(&[("cores", 4)])).is_none(),
            "check and reserve are one critical section"
        );
        drop(guard);
        assert!(l.fits(&res(&[("cores", 8)])), "released on drop");
    }

    #[test]
    fn concurrent_try_reserve_has_one_winner() {
        let ledger = Arc::new(ResourceLedger::new(res(&[("cores", 1)])));
        let start = Arc::new(Barrier::new(3));
        let checked = Arc::new(Barrier::new(3));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for key in ["a", "b"] {
            let ledger = ledger.clone();
            let start = start.clone();
            let checked = checked.clone();
            let winners = winners.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let reservation = ledger.try_reserve(key, &res(&[("cores", 1)]));
                if reservation.is_some() {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
                checked.wait();
                drop(reservation);
            }));
        }

        start.wait();
        checked.wait();
        assert_eq!(winners.load(Ordering::SeqCst), 1);
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(ledger.fits(&res(&[("cores", 1)])));
    }
}
