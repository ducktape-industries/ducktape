//! the node-local worker behind the modules registry's code-swap byte-receipt gate: the state-driven
//! the per-swap byte-receipt readiness signaller for CODE swaps — and the
//! byte PULL lane every member runs beside it.
//!
//! it polls COMMITTED modules registry state (and the OPEN governance
//! proposals naming code) each pump tick. Two duties, split by [`Role`]:
//!
//! FETCH — every member, validator and resident alike: a digest the registry
//! (a pending swap) or an open `RegisterModule`/`UpdateModule` ballot names
//! and this node lacks is reported so the pump spawns a ranged fetch (the
//! custodian's data-plane push normally lands first on a validator; the
//! fetch heals a node the push missed, and is the ONLY lane a resident has,
//! since it never hosts the code plane). A member can taste a proposal's
//! view before the ballot closes only because its own node holds the bytes.
//!
//! SIGNAL — validators only: per pending swap, drive this validator to a
//! truthful `ModulesMsg::SwapReady`:
//!
//! - bytes verified-resident AND loadable on this binary → self-submit ONE
//!   signal (latched locally; the module's committed readiness set keeps it
//!   idempotent across restarts).
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

use std::collections::{BTreeMap, BTreeSet, HashSet};

use sdk::Msg;

use crate::blob_fetch::{BlobFetchError, SourceRotate, fetch_blob};

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

/// what this node IS to the byte-receipt gate — the one discriminant the
/// pump branches on. Both roles PULL the bytes the registry and the open
/// ballots name; only a current validator may SIGNAL, because `SwapReady`
/// is counted against the boundary's readiness quorum and a resident's word
/// is not in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// a current boundary member: fetches, probes loadability, signals.
    Validator,
    /// a member outside the boundary (a parked resident, or a validator
    /// process whose seat rotated out): fetches, nothing else.
    Resident,
}

/// one finished fetch, reported back to the pump that owns the counter.
pub(crate) type FetchOutcome = ([u8; 32], Option<BlobFetchError>);

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
    /// digests the bytes-only path already found present. The presence read
    /// is a VERIFYING one (the whole blob re-hashed) and the pump ticks every
    /// 100 ms, so without this latch an open ballot's artifact would be
    /// re-hashed ten times a second for as long as the ballot stays open.
    /// Trimmed to what the tick still wants: a digest that leaves the wanted
    /// set (its blob is reclaimed) and returns is verified afresh.
    present: BTreeSet<[u8; 32]>,
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
            present: BTreeSet::new(),
        }
    }

    /// the PURE decision core: given this node's role, committed modules
    /// registry status, the digests open ballots name, a local presence read
    /// and a local verdict on each (entry, digest) — the bytes, and the
    /// registry entry (id and kind) they would land under — decide this
    /// tick's signals, fetches and refusals.
    /// truthful (signals only bytes that are held AND run here under that
    /// id), idempotent (committed readiness, the in-flight latch, the fetch
    /// dedupe and the unloadable latch all short-circuit), and quiet once a
    /// swap's `ready_at` has latched.
    ///
    /// the fetch set is bounded: every pending swap (at most one per
    /// registry entry) plus at most [`governance::MAX_PROPOSALS`] proposed
    /// digests, each capped at `MAX_MODULE_CODE_BYTES` by the fetch itself —
    /// the same bound the code plane's push admission carries.
    pub(crate) fn decide(
        &mut self,
        role: Role,
        height: u64,
        modules: &[modules::ModuleCode],
        proposed: &HashSet<[u8; 32]>,
        mut held: impl FnMut(&[u8; 32]) -> bool,
        mut verdict: impl FnMut(&modules::ModuleCode, &[u8; 32]) -> CodeVerdict,
    ) -> CodeActions {
        let mut actions = CodeActions::default();
        let mut wanted = BTreeSet::new();
        for m in modules {
            // a view-only entry's ACTIVE frame has no component, so no fold
            // ever pulls it the way a module's frame is pulled for its code;
            // a node that joined after the register (or was down for it)
            // would hold nothing under the hash the app asks its node for.
            // wanted by every role, signalled by none: the swap that put it
            // there is already decided.
            let view_only = m.kind == modules::Kind::View;
            if view_only && let Ok(active) = <[u8; 32]>::try_from(m.active_code_hash.as_slice()) {
                self.want_bytes(&active, &mut held, &mut wanted, &mut actions);
            }
            let Some(pending) = &m.pending else { continue };
            // past its activation height with no latch: the module itself
            // now refuses to arm this pending on a late signal (it is
            // evictable by governance instead), so fetching or signalling for
            // it is wasted work that never stops on its own.
            if pending.stale_at(height) {
                continue;
            }
            let Ok(digest) = <[u8; 32]>::try_from(pending.code_hash.as_slice()) else {
                continue; // malformed hash can never verify — stay silent.
            };
            // who may answer this pending's readiness probe is the entry's
            // committed kind: a module's bytes take a seat at the boundary and
            // a view's land in the view registry, so a validator says whether
            // they load HERE. A record another plane realizes has no boundary
            // here to answer for it — a validator wants its bytes exactly as a
            // resident does, the R = n latch never closes, and the pending
            // record stays the designation that plane reads.
            let probing_role = match m.kind {
                modules::Kind::Module | modules::Kind::View => role,
                modules::Kind::Plane => Role::Resident,
            };
            match probing_role {
                Role::Resident => self.want_bytes(&digest, &mut held, &mut wanted, &mut actions),
                Role::Validator => self.decide_swap(m, pending, digest, &mut verdict, &mut actions),
            }
        }
        // an open ballot's bytes: wanted by every member, signalled by none —
        // nothing is scheduled until it passes.
        for digest in proposed.iter().take(governance::MAX_PROPOSALS) {
            self.want_bytes(digest, &mut held, &mut wanted, &mut actions);
        }
        self.present.retain(|digest| wanted.contains(digest));
        actions
    }

    /// the bytes-only decision for one digest: absent and not cooling off →
    /// one fetch, deduped against the in-flight set.
    fn want_bytes(
        &mut self,
        digest: &[u8; 32],
        held: &mut impl FnMut(&[u8; 32]) -> bool,
        wanted: &mut BTreeSet<[u8; 32]>,
        actions: &mut CodeActions,
    ) {
        wanted.insert(*digest);
        if self.present.contains(digest) || self.fetching.contains(digest) {
            return;
        }
        let cooling = self
            .failed
            .get(digest)
            .is_some_and(|retry| retry.wait_ticks > 0);
        if cooling {
            return;
        }
        if held(digest) {
            self.present.insert(*digest);
            return;
        }
        self.fetching.insert(*digest);
        actions.fetches.push(*digest);
    }

    /// the validator's decision for one pending swap: signal, fetch, refuse
    /// or stay quiet.
    fn decide_swap(
        &mut self,
        m: &modules::ModuleCode,
        pending: &modules::ScheduledSwap,
        digest: [u8; 32],
        verdict: &mut impl FnMut(&modules::ModuleCode, &[u8; 32]) -> CodeVerdict,
        actions: &mut CodeActions,
    ) {
        // coverage complete: nothing left for anyone to say.
        let coverage_complete = pending.ready_at.is_some();
        if coverage_complete {
            return;
        }
        // the module already recorded our (committed) signal.
        if pending.readiness.iter().any(|k| k == &self.me) {
            return;
        }
        let key = SwapKey {
            module_id: m.module_id.clone(),
            name: pending.name.clone(),
            code_hash: digest,
            activation_height: pending.activation_height,
        };
        if self.signaled.contains(&key) {
            return;
        }
        let latch = (m.module_id.clone(), digest);
        // already refused: this binary will not start loading bytes it
        // could not run, and re-deciding would recompile the component
        // (and re-report the refusal) on every tick until a restart.
        if self.unloadable.contains(&latch) {
            return;
        }
        // the compile is paid ONCE per pair, in either direction: a pair
        // already known to run here skips the probe entirely.
        let answer = match self.loadable.contains(&latch) {
            true => CodeVerdict::Loadable,
            false => verdict(m, &digest),
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

    /// reap every finished fetch: a success clears the digest's slate, a
    /// failure counts, cools the digest off on the backoff, and speaks only
    /// on the first attempt and every Nth after it. Nobody serving these
    /// bytes is the steady state, not a blip: the peer book may be empty
    /// (refused synchronously) and the record naming the bytes never clears
    /// itself, so an unpaced retry+warn here is a permanent ~10/s log bomb
    /// that evicts the ring an operator restarted to read. The warn lives
    /// HERE rather than in the fetch task because this is where the attempt
    /// counter — the actual diagnosis — is.
    pub(crate) fn reap_fetches(
        &mut self,
        done: &mut tokio::sync::mpsc::UnboundedReceiver<FetchOutcome>,
        label: &str,
    ) {
        while let Ok((digest, failure)) = done.try_recv() {
            let Some(error) = failure else {
                self.fetch_succeeded(&digest);
                // once per digest per boot: the bytes a record named are now
                // here, verified — the event a taste or a fold waits on.
                tracing::info!(
                    target: "ducktape::modules",
                    node = %label,
                    digest = %crate::config::hex_bytes(&digest),
                    "module code fetched"
                );
                continue;
            };
            let attempt = self.fetch_failed(&digest);
            if !attempt.speak {
                continue;
            }
            tracing::warn!(
                target: "ducktape::modules",
                node = %label,
                reason = "code_fetch_unserved",
                digest = %crate::config::hex_bytes(&digest),
                attempts = attempt.attempts,
                error = %error,
                "module code fetch failed"
            );
        }
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

/// spawn one ranged, verified fetch per wanted digest; the OUTCOME goes
/// back to the pump over `done`, which owns the attempt counter, the
/// backoff and the (latched) warning. Each fetch is capped at
/// `MAX_MODULE_CODE_BYTES` — the same per-artifact bound the push gate holds.
pub(crate) fn spawn_fetches<C>(
    fetches: Vec<[u8; 32]>,
    client: &C,
    blobs: &blobstore::BlobHandle,
    done: &tokio::sync::mpsc::UnboundedSender<FetchOutcome>,
) where
    C: statesync::SyncClient + SourceRotate + Clone + Send + 'static,
{
    for digest in fetches {
        let client = client.clone();
        let blobs = blobs.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let failure = fetch_blob(
                &client,
                &blobs,
                &digest,
                crate::constants::MAX_MODULE_CODE_BYTES,
                crate::constants::BLOB_FETCH_ATTEMPTS,
            )
            .await
            .err();
            let _ = done.send((digest, failure));
        });
    }
}

/// the digests OPEN `RegisterModule`/`UpdateModule` proposals name, read
/// from governance ONLY when its root has moved since the last read.
///
/// the walk instantiates governance's guest — the same cost class as the
/// registry read the pump already pays each tick (~20 ms measured on a
/// three-node e2e cluster), on the loop that also answers `/v1` and the
/// RPC lane. paying it again every tick buys nothing: governance's root is
/// exactly the change gate for this set, since no ballot opens, closes or
/// moves without it.
///
/// a net with no governance module (no root) names no proposed code, and
/// neither does a reply that is not the listing: an EMPTY set, never a
/// skipped refresh — the registry half drives readiness on its own.
pub(crate) async fn proposed_code_blobs(
    host: &host::Host,
    cache: &mut Option<(sdk::StateRoot, HashSet<[u8; 32]>)>,
) -> HashSet<[u8; 32]> {
    let Some(root) = host.module_root("governance") else {
        *cache = None;
        return HashSet::new();
    };
    if let Some((read_at, digests)) = cache.as_ref()
        && *read_at == root
    {
        return digests.clone();
    }
    let req = governance::encode_query(&governance::GovQuery::Proposals);
    let reply = host.query("governance", &req).await;
    let Ok(Ok(governance::GovReply::Proposals(proposals))) =
        reply.as_deref().map(governance::decode_reply)
    else {
        // a read that failed or answered something else names nothing
        // THIS tick and is retried on the next — never cached, because
        // no root change would invalidate that emptiness.
        return HashSet::new();
    };
    let digests = crate::code_plane::code_blobs_proposed(&proposals);
    *cache = Some((root, digests.clone()));
    digests
}

/// the resident's whole pump: the byte PULL lane a parked node runs once
/// per non-empty drain pass against its served host. It reads the same two
/// records the validator pump reads (registry status, open ballots), decides
/// bytes-only ([`Role::Resident`]: no probe, no signal), and fetches through
/// the ranged mesh lane — the one lane a resident has, since it never hosts
/// the code plane and is never a push fan-out target. The pass IS the tick:
/// the backoff counts passes, never wall time.
pub(crate) struct ResidentCodePull {
    signaller: CodeReadinessSignaller,
    proposed: Option<(sdk::StateRoot, HashSet<[u8; 32]>)>,
    /// the digests the last pass found named — the resident's twin of the
    /// validator's `CodeRegistry`: whatever falls out (a closed ballot, a
    /// cancelled swap) is forgotten, so pulled bytes never outlive the
    /// record that justified them.
    referenced: HashSet<[u8; 32]>,
    done_tx: tokio::sync::mpsc::UnboundedSender<FetchOutcome>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<FetchOutcome>,
}

impl ResidentCodePull {
    pub(crate) fn new() -> Self {
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            // a resident has no readiness identity: nothing here signals.
            signaller: CodeReadinessSignaller::new(Vec::new()),
            proposed: None,
            referenced: HashSet::new(),
            done_tx,
            done_rx,
        }
    }

    /// one pass at served `height`: reap, cool off, read, decide, spawn.
    /// returns the digests this pass started fetching (the test seam; the
    /// fetches themselves report back over the channel).
    pub(crate) async fn pump<C>(
        &mut self,
        label: &str,
        height: u64,
        host: &host::Host,
        blobs: &blobstore::BlobHandle,
        client: &C,
    ) -> Vec<[u8; 32]>
    where
        C: statesync::SyncClient + SourceRotate + Clone + Send + 'static,
    {
        self.signaller.reap_fetches(&mut self.done_rx, label);
        self.signaller.tick_fetch_backoff();
        let req = modules::encode_query(&modules::ModulesQuery::ModuleStatus);
        let Ok(bytes) = host.query(host::MODULES_ID, &req).await else {
            return Vec::new(); // registry absent: nothing names code.
        };
        let Ok(modules::ModulesReply::ModuleStatus { modules }) = modules::decode_reply(&bytes)
        else {
            return Vec::new();
        };
        let proposed = proposed_code_blobs(host, &mut self.proposed).await;
        let mut referenced = crate::code_plane::code_blobs_referenced(&modules);
        referenced.extend(proposed.iter().copied());
        for digest in self.referenced.difference(&referenced) {
            blobs.forget(digest);
        }
        self.referenced = referenced;
        let actions = self.signaller.decide(
            Role::Resident,
            height,
            &modules,
            &proposed,
            |digest| blobs.has_verified_chunk(digest),
            // never asked: the resident arm of `decide` reads presence only.
            |_, _| CodeVerdict::Absent,
        );
        spawn_fetches(actions.fetches.clone(), client, blobs, &self.done_tx);
        actions.fetches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Vec<u8> {
        vec![7; 32]
    }

    fn no_proposals() -> HashSet<[u8; 32]> {
        HashSet::new()
    }

    /// the presence read for a validator-role decide: never consulted for a
    /// pending swap (the verdict answers residency there), so any answer is
    /// as good as none.
    fn never_held(_: &[u8; 32]) -> bool {
        false
    }

    fn proposals(hashes: &[u8]) -> HashSet<[u8; 32]> {
        hashes.iter().map(|h| [*h; 32]).collect()
    }

    /// a registry with no pending swap at all.
    fn idle(module: &str) -> modules::ModuleCode {
        modules::ModuleCode {
            module_id: module.into(),
            kind: modules::Kind::Module,
            active_code_hash: vec![0; 32],
            pending: None,
            history: Vec::new(),
        }
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
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |_, d| loadable(d, 1),
        );
        assert_eq!(acts.signals.len(), 1);
        assert_eq!(acts.signals[0].0, key("held", "replacement", 1));
        assert_eq!(acts.fetches, vec![[2u8; 32]]);

        // second tick: the signal is latched, the fetch deduped.
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |_, d| loadable(d, 1),
        );
        assert!(acts.signals.is_empty());
        assert!(acts.fetches.is_empty());

        // the fetch completes (pump clears the latch) and the bytes are now
        // resident: the swap gets its signal on the next tick.
        s.fetching.clear();
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |_, _| CodeVerdict::Loadable,
        );
        assert_eq!(acts.signals.len(), 1);
        assert_eq!(acts.signals[0].0, key("missing", "replacement", 2));
    }

    /// a `Kind::View` pending is decided like any other: the verdict is
    /// asked with the registry ENTRY (its kind steers what "loadable" means
    /// in the drain — the view ABI alone, no core, no running module asked),
    /// and a loadable answer signals `SwapReady` for it, once.
    #[test]
    fn a_view_pending_is_probed_with_its_kind_and_signals_when_the_view_loads() {
        let mut s = CodeReadinessSignaller::new(me());
        let mut view = pending("home", "deploy-home", 3, false, &[]);
        view.kind = modules::Kind::View;
        let modules = vec![view];
        let mut probed = Vec::new();
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |entry, digest| {
                probed.push((entry.module_id.clone(), entry.kind, *digest));
                CodeVerdict::Loadable
            },
        );
        assert_eq!(
            probed,
            vec![("home".to_string(), modules::Kind::View, [3u8; 32])]
        );
        assert_eq!(acts.signals.len(), 1);
        assert_eq!(acts.signals[0].0, key("home", "deploy-home", 3));
        assert!(acts.refusals.is_empty());
        // latched: the view is not probed again
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |_, _| panic!("a signalled view must not be re-probed"),
        );
        assert!(acts.signals.is_empty());
    }

    /// A VIEW-ONLY ENTRY'S ACTIVE BYTES ARE PULLED, NEVER SIGNALLED. No fold
    /// asks for them (there is no component to run), so a node that joined
    /// after the register holds nothing under the hash its app asks for —
    /// the tab reads "view artifact not held by the node" forever. Every
    /// role wants them; a module entry's active frame is left to the fold.
    #[test]
    fn a_view_only_entry_s_active_bytes_are_pulled_by_every_role() {
        let mut s = CodeReadinessSignaller::new(me());
        let mut view = idle("canvas");
        view.kind = modules::Kind::View;
        view.active_code_hash = vec![9; 32];
        let mut module = idle("boards");
        module.active_code_hash = vec![8; 32];
        let modules = vec![view, module];
        for role in [Role::Resident, Role::Validator] {
            let mut s = CodeReadinessSignaller::new(me());
            let acts = s.decide(role, 1, &modules, &no_proposals(), never_held, |_, _| {
                panic!("an active view is never probed")
            });
            assert_eq!(acts.fetches, vec![[9u8; 32]], "{role:?}");
            assert!(acts.signals.is_empty());
        }
        // held already: nothing to fetch, and the digest stays pinned.
        let acts = s.decide(
            Role::Resident,
            1,
            &modules,
            &no_proposals(),
            |d| d == &[9u8; 32],
            |_, _| CodeVerdict::Absent,
        );
        assert!(acts.fetches.is_empty());
        assert!(s.present.contains(&[9u8; 32]));
    }

    /// A RECORD ANOTHER PLANE REALIZES IS PULLED BY EVERY ROLE AND PROBED BY
    /// NONE. No module boundary here seats its bytes, so no validator can
    /// answer whether they load: `ScheduleRegister`'s R = n latch never
    /// closes, the pending record stays the designation that plane reads, and
    /// a validator wants both its active and its pending frame exactly as a
    /// resident does.
    #[test]
    fn a_plane_entry_is_pulled_by_every_role_and_never_probed() {
        let mut plane = pending("netstack", "netstack-next", 3, false, &[]);
        plane.kind = modules::Kind::Plane;
        plane.active_code_hash = vec![9; 32];
        let modules = vec![plane];
        for role in [Role::Resident, Role::Validator] {
            let mut s = CodeReadinessSignaller::new(me());
            let acts = s.decide(role, 1, &modules, &no_proposals(), never_held, |_, _| {
                panic!("a plane record is never probed")
            });
            assert_eq!(acts.fetches, vec![[9u8; 32], [3u8; 32]], "{role:?}");
            assert!(acts.signals.is_empty(), "{role:?}");
            assert!(acts.refusals.is_empty(), "{role:?}");
        }
    }

    /// BYTE RESIDENCY IS NOT READINESS. A validator whose binary cannot
    /// instantiate the staged component must stay silent: signalling arms a
    /// swap at R = n that this node then deterministically rejects every op
    /// to while its peers apply them (#1297).
    #[test]
    fn code_this_binary_cannot_load_is_never_signalled_and_is_reported_once() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![pending("chat", "replacement", 1, false, &[])];
        let refuse = |_: &modules::ModuleCode, _: &[u8; 32]| CodeVerdict::Unloadable {
            detail: "unknown import `ducktape:module/host@0.2.0`".into(),
        };

        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            refuse,
        );
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
        let acts = s.decide(
            Role::Validator,
            1,
            &modules,
            &no_proposals(),
            never_held,
            |_, _| panic!("a refused digest must not be re-probed"),
        );
        assert!(acts.signals.is_empty());
        assert!(acts.refusals.is_empty());
    }

    #[test]
    fn committed_readiness_and_ready_latch_keep_quiet() {
        let mut s = CodeReadinessSignaller::new(me());
        // our signal already committed: silent.
        let ours = vec![pending("a", "replacement", 1, false, &[me()])];
        assert!(
            s.decide(
                Role::Validator,
                1,
                &ours,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
            .signals
            .is_empty()
        );
        // swap already ready: silent, even though we never signed.
        let armed = vec![pending("b", "replacement", 1, true, &[])];
        let acts = s.decide(
            Role::Validator,
            1,
            &armed,
            &no_proposals(),
            never_held,
            |_, _| CodeVerdict::Loadable,
        );
        assert!(acts.signals.is_empty() && acts.fetches.is_empty());
        // no pending at all: silent.
        let idle = vec![idle("c")];
        let acts = s.decide(
            Role::Validator,
            1,
            &idle,
            &no_proposals(),
            never_held,
            |_, _| CodeVerdict::Loadable,
        );
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
            s.decide(
                Role::Validator,
                1,
                &first,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
            .signals
            .len(),
            1
        );

        // the pending goes stale and the SAME bytes are re-scheduled later.
        let mut replaced = first.clone();
        replaced[0].pending.as_mut().unwrap().activation_height = 200;
        let acts = s.decide(
            Role::Validator,
            1,
            &replaced,
            &no_proposals(),
            never_held,
            |_, _| CodeVerdict::Loadable,
        );
        assert_eq!(acts.signals.len(), 1, "the replacement is a new swap");
        assert_eq!(acts.signals[0].0.activation_height, 200);
        // ...and that one is latched in its own right.
        assert!(
            s.decide(
                Role::Validator,
                1,
                &replaced,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
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
                .decide(
                    Role::Validator,
                    1,
                    &modules,
                    &no_proposals(),
                    never_held,
                    |_, _| CodeVerdict::Absent,
                )
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
            s.decide(
                Role::Validator,
                1,
                &modules,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Absent
            )
            .fetches,
            vec![digest],
            "a cooled-off digest is fetched again"
        );
        s.fetch_succeeded(&digest);
        assert_eq!(
            s.decide(
                Role::Validator,
                1,
                &modules,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
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
            s.decide(
                Role::Validator,
                1,
                &modules,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
            .signals
            .len(),
            1
        );
        assert!(
            s.decide(
                Role::Validator,
                1,
                &modules,
                &no_proposals(),
                never_held,
                |_, _| CodeVerdict::Loadable
            )
            .signals
            .is_empty(),
            "latched"
        );
        s.unlatch(&key("a", "replacement", 1));
        // ...and the retry does NOT re-compile: the digest already answered
        // once, so the probe is paid once per pending swap however many
        // submits fail.
        assert_eq!(
            s.decide(
                Role::Validator,
                1,
                &modules,
                &no_proposals(),
                never_held,
                |_, _| panic!("a digest that already loaded must not be re-probed")
            )
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
        let acts = s.decide(
            Role::Validator,
            11,
            &modules,
            &no_proposals(),
            never_held,
            |_, _| panic!("a stale pending must not be probed at all"),
        );
        assert!(acts.signals.is_empty());
        assert!(acts.fetches.is_empty());
    }

    /// A RESIDENT PULLS AND NEVER SPEAKS. Outside the boundary the same
    /// registry read yields fetches for every pending swap it lacks — the
    /// swap will activate on this node too — and no signal, no probe: a
    /// resident's word is not in the readiness quorum, and compiling the
    /// component here would decide nothing.
    #[test]
    fn a_resident_fetches_pending_bytes_and_signals_nothing() {
        let mut s = CodeReadinessSignaller::new(me());
        let modules = vec![
            pending("held", "replacement", 1, false, &[]),
            pending("missing", "replacement", 2, false, &[]),
            // coverage already complete on the validators: the resident
            // still needs the bytes for the activation it will fold.
            pending("armed", "replacement", 3, true, &[]),
        ];
        let acts = s.decide(
            Role::Resident,
            1,
            &modules,
            &no_proposals(),
            |d| d == &[1u8; 32],
            |_, _| panic!("a resident never probes loadability"),
        );
        assert!(acts.signals.is_empty(), "a resident never signals");
        assert!(acts.refusals.is_empty());
        assert_eq!(acts.fetches, vec![[2u8; 32], [3u8; 32]]);
        // the second tick dedupes against the in-flight set.
        let acts = s.decide(
            Role::Resident,
            1,
            &modules,
            &no_proposals(),
            |d| d == &[1u8; 32],
            |_, _| panic!("a resident never probes loadability"),
        );
        assert!(acts.fetches.is_empty());
        // a stale pending is dead for a resident too.
        let acts = s.decide(
            Role::Resident,
            11,
            &[pending("late", "replacement", 4, false, &[])],
            &no_proposals(),
            |_| false,
            |_, _| CodeVerdict::Absent,
        );
        assert!(acts.fetches.is_empty());
    }

    /// AN OPEN BALLOT'S BYTES ARE WANTED BY EVERY MEMBER. Absent → one fetch,
    /// for a validator and a resident alike; present → nothing, and the
    /// verifying presence read is paid once, not once per tick.
    #[test]
    fn a_proposed_digest_is_fetched_when_absent_and_left_alone_when_held() {
        for role in [Role::Validator, Role::Resident] {
            let mut s = CodeReadinessSignaller::new(me());
            let proposed = proposals(&[5, 6]);
            let mut presence_reads = 0;
            let acts = s.decide(
                role,
                1,
                &[idle("chat")],
                &proposed,
                |d| {
                    presence_reads += 1;
                    d == &[5u8; 32]
                },
                |_, _| panic!("a proposal is never probed for loadability"),
            );
            assert_eq!(acts.fetches, vec![[6u8; 32]], "{role:?}");
            assert!(
                acts.signals.is_empty(),
                "{role:?}: a ballot schedules nothing"
            );
            assert_eq!(presence_reads, 2);
            // next tick: the held digest is latched, the fetch is in flight.
            let acts = s.decide(
                role,
                1,
                &[idle("chat")],
                &proposed,
                |_| panic!("presence is read once per wanted digest"),
                |_, _| panic!("a proposal is never probed for loadability"),
            );
            assert!(acts.fetches.is_empty(), "{role:?}");
            // the fetch lands: the digest is re-read once, then latched.
            s.fetch_succeeded(&[6u8; 32]);
            let mut presence_reads = 0;
            let acts = s.decide(
                role,
                1,
                &[idle("chat")],
                &proposed,
                |_| {
                    presence_reads += 1;
                    true
                },
                |_, _| CodeVerdict::Absent,
            );
            assert!(acts.fetches.is_empty(), "{role:?}");
            assert_eq!(presence_reads, 1);
        }
    }

    /// a settled ballot's digest leaves the wanted set and its presence
    /// latch with it: re-proposed later (after the reclaim forgot the blob),
    /// the bytes are verified — and fetched — afresh.
    #[test]
    fn a_closed_proposal_drops_its_presence_latch() {
        let mut s = CodeReadinessSignaller::new(me());
        let held = s.decide(
            Role::Resident,
            1,
            &[],
            &proposals(&[5]),
            |_| true,
            |_, _| CodeVerdict::Absent,
        );
        assert!(held.fetches.is_empty());
        // closed: nothing wanted, nothing read.
        s.decide(
            Role::Resident,
            1,
            &[],
            &no_proposals(),
            |_| panic!("nothing is wanted"),
            |_, _| CodeVerdict::Absent,
        );
        // re-opened after the reclaim: absent again, so fetched again.
        let again = s.decide(
            Role::Resident,
            1,
            &[],
            &proposals(&[5]),
            |_| false,
            |_, _| CodeVerdict::Absent,
        );
        assert_eq!(again.fetches, vec![[5u8; 32]]);
    }

    /// the proposed half is bounded by governance's own roster cap: a
    /// listing wider than [`governance::MAX_PROPOSALS`] (which the module
    /// itself refuses to grow) never turns into more fetches than that.
    #[test]
    fn proposed_fetches_are_bounded_by_the_proposal_cap() {
        let mut s = CodeReadinessSignaller::new(me());
        let proposed: HashSet<[u8; 32]> = (0..(governance::MAX_PROPOSALS as u32 + 7))
            .map(|i| {
                let mut d = [0u8; 32];
                d[..4].copy_from_slice(&i.to_le_bytes());
                d
            })
            .collect();
        let acts = s.decide(
            Role::Resident,
            1,
            &[],
            &proposed,
            |_| false,
            |_, _| CodeVerdict::Absent,
        );
        assert_eq!(acts.fetches.len(), governance::MAX_PROPOSALS);
    }

    /// a proposed digest that nobody serves backs off exactly like a pending
    /// one: the retry cadence and the warning cadence are the digest's, not
    /// the record's.
    #[test]
    fn an_unserved_proposed_fetch_backs_off() {
        let mut s = CodeReadinessSignaller::new(me());
        let proposed = proposals(&[9]);
        let digest = [9u8; 32];
        let mut fetches = 0;
        let mut warns = 0;
        for _ in 0..6000 {
            if s.fetching.contains(&digest) {
                warns += u32::from(s.fetch_failed(&digest).speak);
            }
            s.tick_fetch_backoff();
            fetches += s
                .decide(
                    Role::Resident,
                    1,
                    &[],
                    &proposed,
                    |_| false,
                    |_, _| CodeVerdict::Absent,
                )
                .fetches
                .len();
        }
        assert!(fetches <= 20, "got {fetches}");
        assert!((1..=3).contains(&warns), "got {warns}");
    }
}
