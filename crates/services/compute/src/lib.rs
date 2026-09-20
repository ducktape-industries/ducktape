//! host-side worker for dispatch saga effects.
//!
//! the dispatch module stages a saga whose spec is a self-described
//! [`WorkSpec`]. this crate is the impure host-side counterpart: it resolves
//! the spec's capability tag to a machine-local [`provider_host::Provider`]
//! (whichever executor CLI the operator brought), feeds the payload to it,
//! and submits the raw answer as a saga `OracleResult` op.
//!
//! the payload is a v1 run ENVELOPE (marker `ducktape_run`; non-current or
//! malformed payloads fail the run loudly), assembled host-side:
//! the instructions, contract and conversation the dispatcher composed. the
//! agent's PERSONA is no longer in there — it is a curated skill, and the
//! provisioner assembles the agent's `always` skills into the run's context
//! document ([`assemble_context_doc`], the SOUL). beyond that
//! assembly the crate stays opinion-free: NO prompt text is authored here,
//! NO output shape is parsed here (the dispatch module judges the recipe's
//! output contract in consensus), and NO credentials are touched (BYO CLI
//! auth). foreign leases are skipped: under the strict policy someone else's
//! assignment would be a no-op result, so not spawning is what turns
//! N-nodes-each-paying-for-the-same-call into one call.
//!
//! the one worker is [`DispatchPool`]: it gates inline ([`gate`]), hands
//! execution to a spawned background task, and returns immediately — so a
//! minutes-long CLI call never stalls the host's event loop.

use std::collections::BTreeMap;

use provider_host::{ProviderOutput, ProviderSet};
use sdk::{Event, Msg};

mod module_contracts;

use module_contracts::{
    AdmissionPolicy, SagaMsg, TokenUsage, WorkSpec, WorkerRequest, decode_work_spec,
    decode_worker_request, encode_saga_msg,
};

pub mod envelope;
mod ledger;
mod pool;
mod provision;
mod soul;
mod workspace_source;
pub use ledger::{ReservationGuard, ResourceLedger};
pub use pool::{
    AttemptControl, CredentialResolver, DeliverFn, DispatchPool, Resolved,
    SharedCredentialResolver, SpawnFn, SpawnKind, max_concurrent_runs_from_env,
};
pub use provision::{
    AgentExecution, ProvisionedWorkspace, RoMount, SharedProvisioner, WorkspaceProvisioner,
    WorkspaceReceipt, WorkspaceSpec,
};
pub use soul::{
    MAX_ALWAYS_BYTES, MAX_DESCRIPTION_CHARS, MAX_INDEXED_SKILLS, SKILL_LIBRARY_PREFIX, SkillDoc,
    assemble_context_doc, parse_skill_md,
};
pub use workspace_source::WorkspaceSource;

/// everything a provider execution needs, extracted by the gate so the
/// expensive run can happen away from the effect-offer callsite (on a spawned
/// task, or inline — the gate does not care).
pub(crate) struct ExecJob {
    pub saga_id: String,
    pub attempt: u32,
    pub capability: String,
    /// the fully rendered prompt — the WorkSpec payload, verbatim.
    pub input: String,
    /// numeric resource demands (Task 4's `WorkSpec.demands`), carried
    /// through so the pool can reserve this run's share of the host-local
    /// [`ResourceLedger`] before it starts executing.
    pub demands: BTreeMap<String, u64>,
    pub admission: AdmissionPolicy,
    /// the PENDING reservation this node's own Accept already took for this
    /// exact attempt, if any — converted into the running reservation on
    /// execution instead of reserving the same demands a second time.
    pub claimed: Option<ReservationGuard>,
}

pub(crate) struct AttemptOutput {
    pub bytes: Vec<u8>,
    pub usage: Option<TokenUsage>,
}

fn attempt_output(output: ProviderOutput, bytes: Vec<u8>) -> AttemptOutput {
    AttemptOutput {
        bytes,
        usage: output.usage.map(|usage| TokenUsage {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
        }),
    }
}

/// the fast, deterministic half of the worker step: decode routing plus the
/// lease gate. everything here is cheap; only [`Gated::Execute`] carries work
/// that costs real time.
pub(crate) enum Gated {
    /// not a dispatch `WorkSpec` effect — offer it to the next worker.
    NotMine,
    /// ours, deliberately not run (foreign lease, or an announcement this
    /// host cannot serve): a claimed skip, carrying WHY as a stable token.
    /// The reason rides the value because the decision is made here and the
    /// logging belongs to the caller — and because a silent skip is
    /// indistinguishable, from outside, from an effect that never arrived.
    Skip(&'static str),
    /// ours, answered without touching a provider: an `Accept` claim for a
    /// servable announcement, or the error `OracleResult` for a lease that
    /// can never execute (non-utf-8 payload, unresolvable capability).
    Immediate(Msg),
    /// our lease, executable: run the provider and submit the result.
    Execute(ExecJob),
}

/// decode + lease-gate one event against this host's provider surface and
/// its process-local load. Announcements require current free capacity; an
/// assigned lease may queue when it fits the node's total capacity.
pub(crate) fn gate(
    providers: &ProviderSet,
    node_key: &[u8],
    ledger: &ResourceLedger,
    event: &Event,
) -> Gated {
    // the pool's own event cadence is the beat a stale pending claim sweeps
    // on (see `ResourceLedger::tick_and_sweep_pending`) — every event this
    // worker is offered, not just a compute one, so a saga that dies with no
    // observable follow-up (cancelled or timed out before any Accept ever
    // landed) still gets swept by unrelated chain traffic.
    ledger.tick_and_sweep_pending();
    let request = match decode_worker_request(&event.payload) {
        Ok(request) => request,
        Err(_) => return Gated::NotMine,
    };
    // the kind-gated decode: foreign spec shapes are NotMine, never a
    // guessed execution.
    let mut work = match decode_work_spec(&request.spec) {
        Ok(work) => work,
        Err(_) => return Gated::NotMine,
    };
    // A missing sandbox dimension is unrestricted, not free. Normalize once
    // so announcement admission, own-lease admission, reservation, and the
    // backend's hard flags all consume the exact same upper-bound map.
    work.demands = ledger.accounted_demands(&work.demands);
    match &request.assignee {
        // the lease gate, host side: someone else's assignment is a
        // claimed skip — it IS our effect type, but the assignee submits
        // the result.
        Some(assignee) if *assignee != node_key => {
            // this node lost the race (or never entered it): the saga
            // assigned the lease elsewhere. release any pending claim this
            // node's own Accept holds for it, so the capacity is free for
            // the next announcement.
            ledger.release_pending(&pending_key(&request));
            Gated::Skip("foreign_lease")
        }
        Some(_) => gate_own_lease(providers, ledger, &request, work),
        // an UNASSIGNED request is an announcement, not a work order:
        // running it would be one execution per capable node. claim it
        // with Accept when this host can actually run the capability AND
        // fit its demands — never claim what cannot fit; the re-emitted
        // request naming the winner is what executes. Queue admission takes
        // a PENDING reservation atomically (not a pure read) so a second
        // announcement offered in the same window sees the capacity as
        // spoken-for; FailFast already reserves atomically at Execute time,
        // so its eligibility check stays the total-capacity read.
        None => {
            let key = pending_key(&request);
            let eligible = match work.admission {
                AdmissionPolicy::Queue => {
                    ledger.claim_pending(key.clone(), &work.demands, request.deadline.is_some())
                }
                AdmissionPolicy::FailFast => ledger.within_capacity(&work.demands),
            };
            if !eligible {
                return Gated::Skip("announcement_over_capacity");
            }
            if providers.resolve(&work.capability).is_err() {
                if work.admission == AdmissionPolicy::Queue {
                    ledger.release_pending(&key);
                }
                return Gated::Skip("announcement_capability_unresolved");
            }
            Gated::Immediate(accept_op(&request))
        }
    }
}

/// the ledger key one `(saga_id, attempt)` reserves under, from the Accept
/// claim through to the running reservation — shared by the announcement's
/// pending claim, its own-lease conversion, and the FailFast atomic reserve
/// so a hand-off never re-reserves the same demands twice.
fn pending_key(request: &WorkerRequest) -> String {
    format!("{}:{}", request.saga_id, request.attempt)
}

/// gate an own-lease request down to an [`ExecJob`] — or the inline error
/// result for a lease that can never execute.
fn gate_own_lease(
    providers: &ProviderSet,
    ledger: &ResourceLedger,
    request: &WorkerRequest,
    work: WorkSpec,
) -> Gated {
    // Convert this node's own pending Accept claim (if any) here: every path
    // below either moves `claimed` into the executed job, or returns without
    // touching it — dropping it and releasing the reservation. A directly
    // pinned lease that was never announced simply finds nothing pending.
    let claimed = ledger.take_pending(&pending_key(request));
    // payload shape first: this verdict must not depend on what happens
    // to be installed on this host.
    let input = match String::from_utf8(work.payload) {
        Ok(input) => input,
        Err(_) => {
            return Gated::Immediate(oracle_result(
                &request.saga_id,
                request.attempt,
                Err(clean_error(
                    "dispatch payload is not utf-8; providers take text".to_string(),
                )),
            ));
        }
    };
    // Total capacity next, BEFORE provider resolve: temporary occupancy queues
    // in the pool, while a lease this node can never fit is a Skip so it can
    // rotate. An error result would consume the attempt with a lie.
    if !ledger.within_capacity(&work.demands) {
        return Gated::Skip("lease_over_capacity");
    }
    if work.admission == AdmissionPolicy::Queue
        && let Err(e) = providers.resolve(&work.capability)
    {
        return Gated::Immediate(oracle_result(
            &request.saga_id,
            request.attempt,
            Err(clean_error(e)),
        ));
    }
    Gated::Execute(ExecJob {
        saga_id: request.saga_id.clone(),
        attempt: request.attempt,
        capability: work.capability,
        input,
        demands: work.demands,
        admission: work.admission,
        claimed,
    })
}

/// the follow-up op that carries one attempt's outcome back through the
/// normal submit path, echoing the request's `(saga_id, attempt)`
/// idempotency key.
fn oracle_result(saga_id: &str, attempt: u32, outcome: Result<Vec<u8>, String>) -> Msg {
    oracle_result_with_usage(saga_id, attempt, outcome, None)
}

fn oracle_result_with_usage(
    saga_id: &str,
    attempt: u32,
    outcome: Result<Vec<u8>, String>,
    usage: Option<TokenUsage>,
) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_saga_msg(&SagaMsg::OracleResult {
            saga_id: saga_id.into(),
            attempt,
            outcome,
            usage,
        }),
    }
}

fn renew_lease(saga_id: &str, attempt: u32) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_saga_msg(&SagaMsg::RenewLease {
            saga_id: saga_id.into(),
            attempt,
        }),
    }
}

/// the claim op for an announcement request — submitted under this node's
/// key; the saga's first-accept-wins rule settles who executes.
fn accept_op(request: &WorkerRequest) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_saga_msg(&SagaMsg::Accept {
            saga_id: request.saga_id.clone(),
            attempt: request.attempt,
        }),
    }
}

/// bound an executor's error text well under saga's error cap.
fn clean_error(error: String) -> String {
    const MAX: usize = 2048;
    if error.len() <= MAX {
        return error;
    }
    let mut keep = MAX;
    while keep > 0 && !error.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut out = error;
    out.truncate(keep);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_contracts::{
        WORK_SPEC_KIND, decode_saga_msg, encode_work_spec, encode_worker_request,
    };

    /// a provider surface with one loaded mock spec and NO installed
    /// binaries — enough for every non-live test; no executor is named.
    fn mock_specs_only() -> ProviderSet {
        let spec = provider_host::CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "alpha"
[detect]
bin = "alpha-cli"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .expect("mock spec parses");
        ProviderSet::assemble(provider_host::SpecSet::from_specs(vec![spec]), Vec::new())
    }

    fn effect_for(spec: Vec<u8>, assignee: Option<&[u8]>) -> Event {
        effect_for_saga("s", spec, assignee)
    }

    fn effect_for_saga(saga_id: &str, spec: Vec<u8>, assignee: Option<&[u8]>) -> Event {
        effect_for_saga_deadline(saga_id, spec, assignee, None)
    }

    fn effect_for_saga_deadline(
        saga_id: &str,
        spec: Vec<u8>,
        assignee: Option<&[u8]>,
        deadline: Option<u64>,
    ) -> Event {
        Event {
            source: "saga".into(),
            payload: encode_worker_request(&WorkerRequest {
                saga_id: saga_id.into(),
                attempt: 0,
                spec,
                deadline,
                assignee: assignee.map(|a| a.to_vec()),
            }),
        }
    }

    fn work_spec() -> Vec<u8> {
        encode_work_spec(&WorkSpec {
            kind: WORK_SPEC_KIND.into(),
            dispatch_id: "d1".into(),
            capability: "alpha".into(),
            payload: b"the entire input".to_vec(),
            demands: Default::default(),
            admission: AdmissionPolicy::Queue,
        })
    }

    #[test]
    fn foreign_specs_are_not_mine_and_foreign_leases_are_skipped() {
        let providers = mock_specs_only();

        // a foreign spec shape (no kind field) is NotMine, never guessed at.
        let foreign = effect_for(br#"{"run_id":"r","agent_id":"a"}"#.to_vec(), None);
        assert!(matches!(
            gate(&providers, b"me", &bare_ledger(), &foreign),
            Gated::NotMine
        ));

        // someone else's lease: claimed but deliberately not run.
        let peer = effect_for(work_spec(), Some(b"peer"));
        assert!(matches!(
            gate(&providers, b"me", &bare_ledger(), &peer),
            Gated::Skip("foreign_lease")
        ));
    }

    #[test]
    fn an_own_lease_with_an_unresolvable_capability_errors_inline() {
        // the spec is loaded but no binary installed: the gate answers with
        // the error result by capability name — never an Execute job.
        let providers = mock_specs_only();
        match gate(
            &providers,
            b"me",
            &bare_ledger(),
            &effect_for(work_spec(), Some(b"me")),
        ) {
            Gated::Immediate(msg) => {
                let SagaMsg::OracleResult { outcome, .. } = decode_saga_msg(&msg.payload).unwrap()
                else {
                    panic!("expected an oracle result");
                };
                let err = outcome.unwrap_err();
                assert!(err.contains("\"alpha\" is not provided"), "got: {err}");
            }
            _ => panic!("an unresolvable own lease must answer inline"),
        }
    }

    /// a provider whose only job is making resolve() succeed in tests.
    struct StubProvider;
    #[async_trait::async_trait]
    impl provider_host::Provider for StubProvider {
        fn capability(&self) -> &str {
            "alpha"
        }
        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            Ok("stub answer".into())
        }
    }

    #[test]
    fn an_unassigned_request_is_claimed_not_run() {
        // an announcement this host CAN serve: the gate answers with an
        // Accept claim, never an execution — the saga's first-accept-wins
        // rule is what turns N capable nodes into one run.
        let providers = servable_providers();
        match gate(
            &providers,
            b"me",
            &bare_ledger(),
            &effect_for(work_spec(), None),
        ) {
            Gated::Immediate(msg) => match decode_saga_msg(&msg.payload).unwrap() {
                SagaMsg::Accept { saga_id, attempt } => {
                    assert_eq!(saga_id, "s");
                    assert_eq!(attempt, 0);
                }
                other => panic!("expected an Accept claim, got {other:?}"),
            },
            _ => panic!("a claimable announcement must produce an op"),
        }

        // an announcement this host CANNOT serve (spec loaded, nothing
        // installed): a quiet skip — never a claim it could not honor.
        let providers = mock_specs_only();
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &bare_ledger(),
                &effect_for(work_spec(), None)
            ),
            Gated::Skip("announcement_capability_unresolved")
        ));
    }

    #[test]
    fn non_utf8_payloads_error_cleanly() {
        let providers = mock_specs_only();
        let spec = encode_work_spec(&WorkSpec {
            kind: WORK_SPEC_KIND.into(),
            dispatch_id: "d1".into(),
            capability: "alpha".into(),
            payload: vec![0xff, 0xfe],
            demands: Default::default(),
            admission: AdmissionPolicy::Queue,
        });
        match gate(
            &providers,
            b"me",
            &bare_ledger(),
            &effect_for(spec, Some(b"me")),
        ) {
            Gated::Immediate(msg) => {
                let SagaMsg::OracleResult { outcome, .. } = decode_saga_msg(&msg.payload).unwrap()
                else {
                    panic!("expected an oracle result");
                };
                assert!(outcome.unwrap_err().contains("not utf-8"));
            }
            _ => panic!("expected a submitted error"),
        }
    }

    /// a ledger with no announced capacity at all — every test above this
    /// point uses demandless specs, so an empty ledger fits them trivially.
    fn bare_ledger() -> ResourceLedger {
        ResourceLedger::new(Default::default())
    }

    fn demands(pairs: &[(&str, u64)]) -> std::collections::BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn work_spec_with_demands(demands: std::collections::BTreeMap<String, u64>) -> Vec<u8> {
        encode_work_spec(&WorkSpec {
            kind: WORK_SPEC_KIND.into(),
            dispatch_id: "d1".into(),
            capability: "alpha".into(),
            payload: b"the entire input".to_vec(),
            demands,
            admission: AdmissionPolicy::Queue,
        })
    }

    /// a provider surface that can actually run "alpha" — the servable twin
    /// of `mock_specs_only`, for tests where the capacity check must be what
    /// produces the Skip (not a missing provider).
    fn servable_providers() -> ProviderSet {
        let spec = provider_host::CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "alpha"
[detect]
bin = "alpha-cli"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .expect("mock spec parses");
        ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec]),
            vec![Box::new(StubProvider)],
        )
    }

    #[test]
    fn an_own_lease_over_capacity_is_skipped_so_the_lease_can_rotate() {
        // providers resolve, but ledger capacity {"cores": 4} < demands
        // {"cores": 8} -> Gated::Skip (NOT an inline error: the saga lease
        // expires and the next attempt rendezvouses elsewhere; an error
        // result would consume the attempt with a lie — this node was never
        // able to run it).
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 4)]));
        let spec = work_spec_with_demands(demands(&[("cores", 8)]));
        assert!(matches!(
            gate(&providers, b"me", &ledger, &effect_for(spec, Some(b"me"))),
            Gated::Skip("lease_over_capacity")
        ));
    }

    #[test]
    fn an_announcement_over_capacity_is_not_claimed() {
        // unassigned request + over-capacity demands -> Gated::Skip, never
        // Accept.
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 4)]));
        let spec = work_spec_with_demands(demands(&[("cores", 8)]));
        assert!(matches!(
            gate(&providers, b"me", &ledger, &effect_for(spec, None)),
            Gated::Skip("announcement_over_capacity")
        ));
    }

    #[test]
    fn assigned_work_queues_behind_occupancy_but_announcements_do_not_claim() {
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 4)]));
        let _running = ledger
            .try_reserve("running", &demands(&[("cores", 4)]))
            .expect("first run fills capacity");
        let spec = work_spec_with_demands(demands(&[("cores", 4)]));

        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for(spec.clone(), Some(b"me"))
            ),
            Gated::Execute(_)
        ));
        assert!(matches!(
            gate(&providers, b"me", &ledger, &effect_for(spec, None)),
            Gated::Skip("announcement_over_capacity")
        ));
    }

    #[test]
    fn fail_fast_announcements_use_total_capacity_and_carry_policy_to_execution() {
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 4)]));
        let _running = ledger
            .try_reserve("running", &demands(&[("cores", 4)]))
            .expect("first run fills capacity");
        let mut work: WorkSpec =
            decode_work_spec(&work_spec_with_demands(demands(&[("cores", 4)]))).expect("work spec");
        work.admission = AdmissionPolicy::FailFast;
        let spec = encode_work_spec(&work);

        assert!(matches!(
            gate(&providers, b"me", &ledger, &effect_for(spec.clone(), None)),
            Gated::Immediate(_)
        ));
        let Gated::Execute(job) = gate(&providers, b"me", &ledger, &effect_for(spec, Some(b"me")))
        else {
            panic!("assigned fail-fast work reaches the atomic admission step")
        };
        assert_eq!(job.admission, AdmissionPolicy::FailFast);
    }

    #[test]
    fn demandless_jobs_bypass_capacity_even_on_a_bare_ledger() {
        // empty demands + empty capacity -> Gated::Execute as today.
        let providers = servable_providers();
        let ledger = bare_ledger();
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for(work_spec(), Some(b"me"))
            ),
            Gated::Execute(_)
        ));
    }

    #[test]
    fn omitted_sandbox_dimensions_become_full_capacity_demands() {
        let providers = servable_providers();
        let capacity = demands(&[("cores", 8), ("mem_gb", 16)]);
        let ledger = ResourceLedger::new(capacity.clone());
        let Gated::Execute(job) = gate(
            &providers,
            b"me",
            &ledger,
            &effect_for(work_spec(), Some(b"me")),
        ) else {
            panic!("a demandless sandbox run should execute with full accounting")
        };
        assert_eq!(job.demands, capacity);

        let partial = work_spec_with_demands(demands(&[("cores", 2)]));
        let Gated::Execute(job) = gate(
            &providers,
            b"me",
            &ledger,
            &effect_for(partial, Some(b"me")),
        ) else {
            panic!("a partial sandbox run should fill its omitted dimensions")
        };
        assert_eq!(job.demands, demands(&[("cores", 2), ("mem_gb", 16)]));
    }

    #[test]
    fn only_one_of_four_same_window_announcements_is_accepted() {
        // capacity 8, four different sagas each announcing a demand of 8 in
        // the same window: a pure `fits` read would accept all four since
        // nothing is reserved between offers. Claiming a pending reservation
        // on Accept must make exactly one of them fit.
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 8)]));
        let accepted = (0..4)
            .filter(|i| {
                let spec = work_spec_with_demands(demands(&[("cores", 8)]));
                let event = effect_for_saga(&format!("s{i}"), spec, None);
                matches!(
                    gate(&providers, b"me", &ledger, &event),
                    Gated::Immediate(_)
                )
            })
            .count();
        assert_eq!(accepted, 1, "only one announcement may claim the capacity");
    }

    #[test]
    fn a_lost_race_releases_its_pending_claim_for_a_later_announcement() {
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 8)]));
        let spec0 = work_spec_with_demands(demands(&[("cores", 8)]));
        let spec1 = work_spec_with_demands(demands(&[("cores", 8)]));

        // this node accepts s0's announcement, claiming all 8 cores.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s0", spec0.clone(), None)
            ),
            Gated::Immediate(_)
        ));
        // a second announcement in the same window finds no free capacity.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s1", spec1.clone(), None)
            ),
            Gated::Skip("announcement_over_capacity")
        ));

        // the saga assigns s0's lease to another node: this node lost the
        // race and must release its pending claim.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s0", spec0, Some(b"other"))
            ),
            Gated::Skip("foreign_lease")
        ));

        // capacity is free again: the later announcement is now accepted.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s1", spec1, None)
            ),
            Gated::Immediate(_)
        ));
    }

    #[test]
    fn a_won_announcement_converts_its_pending_claim_without_double_reserving() {
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 8)]));
        let spec = work_spec_with_demands(demands(&[("cores", 8)]));

        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s0", spec.clone(), None)
            ),
            Gated::Immediate(_)
        ));

        // the saga re-emits the request naming this node the winner: the
        // pending claim converts into the executed job's reservation.
        let Gated::Execute(job) = gate(
            &providers,
            b"me",
            &ledger,
            &effect_for_saga("s0", spec, Some(b"me")),
        ) else {
            panic!("a won announcement converts to Execute")
        };
        assert!(
            job.claimed.is_some(),
            "the converted job carries the pending reservation"
        );
        // the capacity is held exactly once by the converted reservation —
        // a double reservation would already have exhausted it twice over.
        assert!(!ledger.fits(&demands(&[("cores", 1)])));
    }

    /// an announcement whose demand names a dimension this ledger's capacity
    /// never declared — always a deterministic Skip regardless of any
    /// "cores" occupancy, so it can drive the pool's beat (gate() ticks it
    /// on every call, before decoding) without claiming or contending for
    /// capacity itself.
    fn noise_event() -> Event {
        effect_for_saga(
            "noise",
            work_spec_with_demands(demands(&[("gpu", 1)])),
            None,
        )
    }

    #[test]
    fn a_saga_cancelled_before_assignment_is_swept_after_its_beat_budget() {
        // s0 is cancelled (or times out) before any Accept ever lands, so
        // its `assignee` never leaves `None`: the saga's own
        // `cancel_attempt`/`Crank` deliberately emit nothing for an
        // unassigned attempt, so no event ever reaches this node telling it
        // to release the claim. The bounded backstop is the only thing that
        // frees the capacity.
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 8)]));
        let full_demand = || work_spec_with_demands(demands(&[("cores", 8)]));

        // this node accepts s0's announcement; it carries a saga deadline,
        // so the claim is bounded.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga_deadline("s0", full_demand(), None, Some(1)),
            ),
            Gated::Immediate(_)
        ));

        // capacity is claimed: an unrelated announcement in the same window
        // does not fit.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s1", full_demand(), None)
            ),
            Gated::Skip("announcement_over_capacity")
        ));

        // s0 never gets a follow-up. enough unrelated chain traffic passes
        // (the pool's own beat, driven here by demand-mismatched noise that
        // never itself claims anything) for the backstop to sweep it.
        for _ in 0..ledger::PENDING_CLAIM_BEAT_BUDGET {
            assert!(matches!(
                gate(&providers, b"me", &ledger, &noise_event()),
                Gated::Skip("announcement_over_capacity")
            ));
        }

        // capacity is free again: the same announcement is now accepted.
        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s1", full_demand(), None)
            ),
            Gated::Immediate(_)
        ));
    }

    #[test]
    fn an_unbounded_pending_claim_never_sweeps() {
        // an announcement with no saga deadline mirrors the saga's own "no
        // deadline, no lease: never expires" rule — the local beat backstop
        // must not free it either, no matter how much traffic passes.
        let providers = servable_providers();
        let ledger = ResourceLedger::new(demands(&[("cores", 8)]));
        let full_demand = || work_spec_with_demands(demands(&[("cores", 8)]));

        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s0", full_demand(), None)
            ),
            Gated::Immediate(_)
        ));

        for _ in 0..ledger::PENDING_CLAIM_BEAT_BUDGET * 2 {
            let _ = gate(&providers, b"me", &ledger, &noise_event());
        }

        assert!(matches!(
            gate(
                &providers,
                b"me",
                &ledger,
                &effect_for_saga("s1", full_demand(), None)
            ),
            Gated::Skip("announcement_over_capacity")
        ));
    }
}
