//! the node-local worker behind the modules registry's code-swap byte-receipt gate: the state-driven
//! the per-swap byte-receipt readiness signaller for CODE swaps.
//!
//! it polls COMMITTED modules registry state each pump tick and, per pending swap,
//! drives this validator to a truthful `ModulesMsg::SwapReady`:
//!
//! - bytes verified-resident AND loadable on this binary → self-submit ONE
//!   signal (latched locally; the module's committed readiness set keeps it
//!   idempotent across restarts).
//! - bytes absent → report the digest so the pump spawns a ranged fetch
//!   (the custodian's data-plane push normally lands first; the fetch heals
//!   a node the push missed).
//! - bytes held but not loadable on this binary → report the refusal once and
//!   stay silent. Never signal what is not held AND not runnable: "ready" is a
//!   machine statement that `sha256(local bytes) == committed hash` and that
//!   THIS build can instantiate them. Residency alone let a validator on an
//!   older binary arm a swap it then deterministically rejected every op to
//!   while its peers applied them — a silent fork (#1297).
//!
//! deliberately NOT a `host::worker::Worker` (same reasoning as the upgrade
//! signaller): readiness must survive restart/late-join, so every decision
//! re-derives from committed state instead of reacting to one-shot effects.

use std::collections::{BTreeMap, BTreeSet};

use sdk::Msg;

/// the ceiling on the fetch backoff, in DRAIN TICKS (the pump runs once per
/// `DRAIN_TICK`, so 600 is a minute). Nobody serving the bytes is not a
/// transient: the custodian may be down for hours and the pending swap is
/// never cleared by the module, so the retry has to settle into a cadence a
/// node can hold forever — while still healing within a minute of the bytes
/// appearing.
const FETCH_BACKOFF_MAX_TICKS: u32 = 600;

/// one warning per failing digest, then one per this many further failures.
/// The attempt COUNTER is the diagnosis; the repetition is a log bomb that
/// evicts the 4096-line ring roughly every 7 minutes.
const FETCH_WARN_EVERY: u32 = 10;

/// what one failed fetch earns: the attempt number to report, and whether this
/// attempt is one of the ones that speaks.
pub(crate) struct FetchFailure {
    pub(crate) attempts: u32,
    pub(crate) speak: bool,
}

/// a digest whose last fetch failed: how many times, and how many drain ticks
/// remain before the next attempt.
struct FetchRetry {
    attempts: u32,
    wait_ticks: u32,
}

/// ONE pending swap's identity — the whole of it. A name is reusable: a stale
/// pending is replaceable under the same name, by the same bytes at a new
/// activation height (the module CLI derives the name FROM the hash, so a retry
/// of the same artifact always does exactly that). Keyed by name alone, this
/// node's in-flight latch would silence it for the replacement forever.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SwapKey {
    pub(crate) module_id: String,
    pub(crate) name: String,
    pub(crate) code_hash: [u8; 32],
    pub(crate) activation_height: u64,
}

/// everything this node can truthfully say about one pending swap's bytes —
/// ONE discriminant, so a new answer has to be routed rather than defaulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodeVerdict {
    /// held, content-verified, AND this binary instantiates them: the only
    /// state that may sign `SwapReady`.
    Loadable,
    /// held, but this binary cannot instantiate them — a host import this
    /// build does not provide, or a component encoding it does not speak.
    Unloadable { detail: String },
    /// not held locally.
    Absent,
}

pub(crate) struct CodeReadinessSignaller {
    /// this node's own validator pubkey bytes — the readiness identity.
    me: Vec<u8>,
    /// swaps a signal is already in flight for (submitted, awaiting
    /// finalization) — local dedupe atop the module's idempotence.
    pub(crate) signaled: BTreeSet<SwapKey>,
    /// digests a fetch is already running for — cleared by the pump when a
    /// fetch task finishes (either way), so a failed fetch retries.
    pub(crate) fetching: BTreeSet<[u8; 32]>,
    /// digests whose last fetch FAILED, and are cooling off. Every failure
    /// path here is fast, not slow — an empty blob peer book refuses
    /// synchronously and an honest miss answers in one RTT — so without a
    /// backoff the pump respawned a fetch (and warned) every 100 ms forever
    /// for bytes nobody holds.
    failed: BTreeMap<[u8; 32], FetchRetry>,
    /// (module id, digest) pairs this binary already refused. THE LATCH:
    /// loading a component compiles it, and the answer — can these bytes run
    /// HERE, under THIS id (the substrate the id offers) — cannot change
    /// while this process lives, so the refusal is decided, reported, and
    /// never re-paid.
    unloadable: BTreeSet<(String, [u8; 32])>,
    /// the other half of that latch: pairs that already answered loadable
    /// here. The signal latch is keyed by swap and `unlatch`ed when a submit
    /// fails, so without this a failed submit would re-read and RE-COMPILE
    /// the same bytes on the next tick.
    loadable: BTreeSet<(String, [u8; 32])>,
}

/// what one pump tick should do: signals to submit, fetches to spawn, refusals
/// to report.
#[derive(Default)]
pub(crate) struct CodeActions {
    pub(crate) signals: Vec<(SwapKey, Msg)>,
    pub(crate) fetches: Vec<[u8; 32]>,
    /// swaps whose bytes this binary cannot run, with the loader's own words —
    /// emitted ONCE per digest by the `unloadable` latch.
    pub(crate) refusals: Vec<(SwapKey, String)>,
}

impl CodeReadinessSignaller {
    pub(crate) fn new(me: Vec<u8>) -> Self {
        Self {
            me,
            signaled: BTreeSet::new(),
            fetching: BTreeSet::new(),
            failed: BTreeMap::new(),
            unloadable: BTreeSet::new(),
            loadable: BTreeSet::new(),
        }
    }

    /// the PURE decision core: given committed modules registry status and a
    /// local verdict on each (module id, digest) — the bytes, and the id they
    /// would run under — decide this tick's signals, fetches and refusals.
    /// truthful (signals only bytes that are held AND run here under that
    /// id), idempotent (committed readiness, the in-flight latch, the fetch
    /// dedupe and the unloadable latch all short-circuit), and quiet once a
    /// swap's `ready_at` has latched.
    pub(crate) fn decide(
        &mut self,
        height: u64,
        modules: &[modules::ModuleCode],
        mut verdict: impl FnMut(&str, &[u8; 32]) -> CodeVerdict,
    ) -> CodeActions {
        let mut actions = CodeActions::default();
        for m in modules {
            let Some(pending) = &m.pending else { continue };
            // coverage complete: nothing left for anyone to say.
            let coverage_complete = pending.ready_at.is_some();
            if coverage_complete {
                continue;
            }
            // past its activation height with no latch: the module itself
            // now refuses to arm this pending on a late signal (it is
            // evictable by governance instead), so fetching or signalling for
            // it is wasted work that never stops on its own.
            if pending.stale_at(height) {
                continue;
            }
            // the module already recorded our (committed) signal.
            if pending.readiness.iter().any(|k| k == &self.me) {
                continue;
            }
            let Ok(digest) = <[u8; 32]>::try_from(pending.code_hash.as_slice()) else {
                continue; // malformed hash can never verify — stay silent.
            };
            let key = SwapKey {
                module_id: m.module_id.clone(),
                name: pending.name.clone(),
                code_hash: digest,
                activation_height: pending.activation_height,
            };
            if self.signaled.contains(&key) {
                continue;
            }
            let latch = (m.module_id.clone(), digest);
            // already refused: this binary will not start loading bytes it
            // could not run, and re-deciding would recompile the component
            // (and re-report the refusal) on every tick until a restart.
            if self.unloadable.contains(&latch) {
                continue;
            }
            // the compile is paid ONCE per pair, in either direction: a pair
            // already known to run here skips the probe entirely.
            let answer = match self.loadable.contains(&latch) {
                true => CodeVerdict::Loadable,
                false => verdict(&m.module_id, &digest),
            };
            match answer {
                CodeVerdict::Loadable => {
                    self.loadable.insert(latch);
                    self.signaled.insert(key.clone());
                    let msg = Msg {
                        target: host::MODULES_ID.into(),
                        payload: modules::encode_msg(&modules::ModulesMsg::SwapReady {
                            name: key.name.clone(),
                            module_id: key.module_id.clone(),
                            code_hash: digest.to_vec(),
                        }),
                    };
                    actions.signals.push((key, msg));
                }
                CodeVerdict::Unloadable { detail } => {
                    self.unloadable.insert(latch);
                    actions.refusals.push((key, detail));
                }
                CodeVerdict::Absent => {
                    let cooling = self
                        .failed
                        .get(&digest)
                        .is_some_and(|retry| retry.wait_ticks > 0);
                    if !cooling && self.fetching.insert(digest) {
                        actions.fetches.push(digest);
                    }
                }
            }
        }
        actions
    }

    /// one drain tick's worth of cooling for every digest waiting to retry.
    /// The cadence is the pump's own tick count, never a timer: a retry that
    /// slept would fire while the loop was inside a 60 s checkpoint.
    pub(crate) fn tick_fetch_backoff(&mut self) {
        for retry in self.failed.values_mut() {
            retry.wait_ticks = retry.wait_ticks.saturating_sub(1);
        }
    }

    /// a fetch finished with an error: count it, cool the digest off for
    /// exponentially longer (capped), and say whether this attempt speaks.
    pub(crate) fn fetch_failed(&mut self, digest: &[u8; 32]) -> FetchFailure {
        self.fetching.remove(digest);
        let retry = self.failed.entry(*digest).or_insert(FetchRetry {
            attempts: 0,
            wait_ticks: 0,
        });
        retry.attempts = retry.attempts.saturating_add(1);
        retry.wait_ticks = 1u32
            .checked_shl(retry.attempts.min(16))
            .unwrap_or(FETCH_BACKOFF_MAX_TICKS)
            .min(FETCH_BACKOFF_MAX_TICKS);
        FetchFailure {
            attempts: retry.attempts,
            speak: retry.attempts == 1 || retry.attempts.is_multiple_of(FETCH_WARN_EVERY),
        }
    }

    /// a fetch landed the bytes: the digest owes nothing and starts clean if
    /// it is ever asked for again.
    pub(crate) fn fetch_succeeded(&mut self, digest: &[u8; 32]) {
        self.fetching.remove(digest);
        self.failed.remove(digest);
    }

    /// un-latch a swap whose signal submit failed, so the next tick retries.
    pub(crate) fn unlatch(&mut self, key: &SwapKey) {
        self.signaled.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Vec<u8> {
        vec![7; 32]
    }

    /// the key `pending()` produces for one module/name/hash at the default
    /// activation height.
    fn key(module: &str, name: &str, hash: u8) -> SwapKey {
        SwapKey {
            module_id: module.into(),
            name: name.into(),
            code_hash: [hash; 32],
            activation_height: 10,
        }
    }

    fn pending(
        module: &str,
        name: &str,
        hash: u8,
        ready: bool,
        signed: &[Vec<u8>],
    ) -> modules::ModuleCode {
        modules::ModuleCode {
            module_id: module.into(),
            kind: modules::Kind::Module,
            active_code_hash: vec![0; 32],
            pending: Some(modules::ScheduledSwap {
                name: name.into(),
                activation_height: 10,
                code_hash: vec![hash; 32],
                readiness: signed.to_vec(),
                ready_at: ready.then_some(5),
            }),
            history: Vec::new(),
        }
    }

    /// held bytes that load here.
    fn loadable(digest: &[u8; 32], held: u8) -> CodeVerdict {
        match digest == &[held; 32] {
            true => CodeVerdict::Loadable,
            false => CodeVerdict::Absent,
        }
    }

    #[test]
    fn resident_bytes_signal_once_absent_bytes_fetch_once() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![
            pending("held", "replacement", 1, false, &[]),
            pending("missing", "replacement", 2, false, &[]),
        ];
        let acts = s.decide(1, &modules, |_, d| loadable(d, 1));
        assert_eq!(acts.signals.len(), 1);
        assert_eq!(acts.signals[0].0, key("held", "replacement", 1));
        assert_eq!(acts.fetches, vec![[2u8; 32]]);

        // second tick: the signal is latched, the fetch deduped.
        let acts = s.decide(1, &modules, |_, d| loadable(d, 1));
        assert!(acts.signals.is_empty());
        assert!(acts.fetches.is_empty());

        // the fetch completes (pump clears the latch) and the bytes are now
        // resident: the swap gets its signal on the next tick.
        s.fetching.clear();
        let acts = s.decide(1, &modules, |_, _| CodeVerdict::Loadable);
        assert_eq!(acts.signals.len(), 1);
        assert_eq!(acts.signals[0].0, key("missing", "replacement", 2));
    }

    /// BYTE RESIDENCY IS NOT READINESS. A validator whose binary cannot
    /// instantiate the staged component must stay silent: signalling arms a
    /// swap at R = n that this node then deterministically rejects every op
    /// to while its peers apply them (#1297).
    #[test]
    fn code_this_binary_cannot_load_is_never_signalled_and_is_reported_once() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![pending("chat", "replacement", 1, false, &[])];
        let refuse = |_: &str, _: &[u8; 32]| CodeVerdict::Unloadable {
            detail: "unknown import `ducktape:module/host@0.2.0`".into(),
        };

        let acts = s.decide(1, &modules, refuse);
        assert!(
            acts.signals.is_empty(),
            "holding bytes this binary cannot run is not readiness"
        );
        assert!(
            acts.fetches.is_empty(),
            "the bytes are here; fetching is not the fix"
        );
        assert_eq!(acts.refusals.len(), 1);
        assert_eq!(acts.refusals[0].0, key("chat", "replacement", 1));

        // LATCHED: loading compiles the component and the answer cannot change
        // while this process lives, so neither the work nor the log repeats.
        let acts = s.decide(1, &modules, |_, _| {
            panic!("a refused digest must not be re-probed")
        });
        assert!(acts.signals.is_empty());
        assert!(acts.refusals.is_empty());
    }

    #[test]
    fn committed_readiness_and_ready_latch_keep_quiet() {
        let mut s = CodeReadinessSignaller::new(me());
        // our signal already committed: silent.
        let ours = vec![pending("a", "replacement", 1, false, &[me()])];
        assert!(
            s.decide(1, &ours, |_, _| CodeVerdict::Loadable)
                .signals
                .is_empty()
        );
        // swap already ready: silent, even though we never signed.
        let armed = vec![pending("b", "replacement", 1, true, &[])];
        let acts = s.decide(1, &armed, |_, _| CodeVerdict::Loadable);
        assert!(acts.signals.is_empty() && acts.fetches.is_empty());
        // no pending at all: silent.
        let idle = vec![modules::ModuleCode {
            module_id: "c".into(),
            kind: modules::Kind::Module,
            active_code_hash: vec![0; 32],
            pending: None,
            history: Vec::new(),
        }];
        let acts = s.decide(1, &idle, |_, _| CodeVerdict::Loadable);
        assert!(acts.signals.is_empty() && acts.fetches.is_empty());
    }

    /// A REPLACED PENDING IS A NEW SWAP. A stale schedule is replaceable under
    /// the same name by the same bytes at a new activation height — exactly
    /// what re-running the module CLI on one artifact produces. A latch keyed
    /// by name alone silenced this validator for the replacement for the life
    /// of the process, so the retry could never latch readiness.
    #[test]
    fn a_rescheduled_swap_under_the_same_name_signals_again() {
        let mut s = CodeReadinessSignaller::new(me());
        let first = vec![pending("chat", "chat@ab12", 1, false, &[])];
        assert_eq!(
            s.decide(1, &first, |_, _| CodeVerdict::Loadable)
                .signals
                .len(),
            1
        );

        // the pending goes stale and the SAME bytes are re-scheduled later.
        let mut replaced = first.clone();
        replaced[0].pending.as_mut().unwrap().activation_height = 200;
        let acts = s.decide(1, &replaced, |_, _| CodeVerdict::Loadable);
        assert_eq!(acts.signals.len(), 1, "the replacement is a new swap");
        assert_eq!(acts.signals[0].0.activation_height, 200);
        // ...and that one is latched in its own right.
        assert!(
            s.decide(1, &replaced, |_, _| CodeVerdict::Loadable)
                .signals
                .is_empty()
        );
    }

    /// BYTES NOBODY SERVES ARE A STEADY STATE, NOT A BLIP. The failure path is
    /// fast (an empty peer book refuses synchronously) and the module never
    /// clears a pending swap, so an unpaced pump respawned a fetch and warned
    /// on every 100 ms drain tick forever: ~10 warns/s turned the 4096-line
    /// ring over every ~7 minutes and issued ~30 BlobInfo requests/s at peers
    /// for a blob that does not exist.
    ///
    /// Drain ticks are counted, never slept: the pump's own tick IS the clock,
    /// so this walks ten minutes of a wedged node in microseconds.
    #[test]
    fn an_unservable_fetch_backs_off_and_stops_shouting() {
        const TEN_MINUTES_OF_TICKS: u32 = 6000;
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![pending("missing", "replacement", 2, false, &[])];
        let digest = [2u8; 32];

        let mut fetches = 0;
        let mut warns = 0;
        for _ in 0..TEN_MINUTES_OF_TICKS {
            // the pump: reap the failed fetch, cool it off one tick, decide.
            if s.fetching.contains(&digest) {
                let failure = s.fetch_failed(&digest);
                warns += u32::from(failure.speak);
            }
            s.tick_fetch_backoff();
            fetches += s
                .decide(1, &modules, |_, _| CodeVerdict::Absent)
                .fetches
                .len();
        }

        assert!(
            fetches <= 20,
            "ten minutes of a blob nobody holds must cost a handful of \
             fetches, not one per tick — got {fetches}"
        );
        assert!(
            (1..=3).contains(&warns),
            "the first failure speaks, then one per {FETCH_WARN_EVERY} — \
             got {warns} warnings"
        );

        // ...and the moment the bytes appear the swap signals: the backoff
        // delays a retry, it never gives up on one.
        for _ in 0..FETCH_BACKOFF_MAX_TICKS {
            s.tick_fetch_backoff();
        }
        assert_eq!(
            s.decide(1, &modules, |_, _| CodeVerdict::Absent).fetches,
            vec![digest],
            "a cooled-off digest is fetched again"
        );
        s.fetch_succeeded(&digest);
        assert_eq!(
            s.decide(1, &modules, |_, _| CodeVerdict::Loadable)
                .signals
                .len(),
            1
        );
    }

    #[test]
    fn unlatch_retries_a_failed_submit() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![pending("a", "replacement", 1, false, &[])];
        assert_eq!(
            s.decide(1, &modules, |_, _| CodeVerdict::Loadable)
                .signals
                .len(),
            1
        );
        assert!(
            s.decide(1, &modules, |_, _| CodeVerdict::Loadable)
                .signals
                .is_empty(),
            "latched"
        );
        s.unlatch(&key("a", "replacement", 1));
        // ...and the retry does NOT re-compile: the digest already answered
        // once, so the probe is paid once per pending swap however many
        // submits fail.
        assert_eq!(
            s.decide(1, &modules, |_, _| panic!(
                "a digest that already loaded must not be re-probed"
            ))
            .signals
            .len(),
            1,
            "retries"
        );
    }

    /// a pending past its activation_height with no latch is DEAD (#1676): a
    /// signaller given that height must neither keep fetching nor keep
    /// signalling for it — the forever-loop `decide` was stuck in when it had
    /// no height to judge staleness against.
    #[test]
    fn a_stale_pending_is_never_fetched_or_signalled() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![pending("missing", "replacement", 2, false, &[])];
        // height 11 > activation_height 10, never latched: stale.
        let acts = s.decide(11, &modules, |_, _| {
            panic!("a stale pending must not be probed at all")
        });
        assert!(acts.signals.is_empty());
        assert!(acts.fetches.is_empty());
    }
}
