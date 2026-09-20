//! The governance trigger for the reachability plane's backend: a node-local
//! reconciler that converges this node's netstack machine onto the component
//! the module code registry designates under the `netstack` id.
//!
//! WHY IT IS NOT THE MODULE BOUNDARY. `Host::realize_module_swaps` realizes
//! consensus module code: it is fail-closed by construction (a node that
//! cannot run the agreed code must not apply the block) and it runs INSIDE the
//! drain. The reachability machine is neither — it is sans-I/O, per-node,
//! pre-genesis networking that contributes no root-hash, and a swap is
//! accepted at any event boundary with no cross-validator synchrony
//! requirement. So the registry is used here as the COMMITMENT RECORD ONLY
//! (governance's existing `RegisterModule` / `UpdateModule` wire surface, no
//! new action, no wire change) and this task — off the drain, off the select
//! loop — reads it and drives the same `swap_netstack()` conversion the admin
//! route drives. Nothing here can defer a frame or return `Err` to the drain.
//!
//! THE DESIGNATION IS THE PENDING RECORD, AT ITS ACTIVATION HEIGHT. A
//! `ducktape:netstack` component is not a `ducktape:module`, so no validator's
//! readiness probe can load it and `ScheduleRegister`'s R = n latch never
//! closes for it: the entry stays pending, and the pending hash IS what
//! governance designated. The HEIGHT half of the schedule is honoured
//! regardless — governance schedules a swap AT a height and the registry's
//! minimum swap lead exists so every node cuts over on the same block. (The
//! module boundary skips such a record outright — see
//! `Host::skip_foreign_admission`.)
//!
//! ONE SWAP PER DESIGNATION — spent by a MACHINE'S ANSWER, not by an attempt.
//! A backend that refuses the swap (a component built against another
//! contract, refused by name before a byte of state is decoded) refuses it
//! identically every time and keeps running untouched, so that refusal is said
//! once per actual plane execution. Recreating the plane or changing its
//! running component reopens that designation. The two
//! non-answers heal on their own and retry on the next block: bytes this node
//! does not hold yet (the code plane's push and the readiness pump's fetch
//! land them), and a swap no plane was running to answer.

use futures::SinkExt as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::broadcast::error::RecvError;

use crate::module_contracts::modules;
use crate::reachability_plane::SwapAnswer;

/// the module code registry id the reachability component is committed under.
/// Deliberately absent from `topology::PRODUCTION`: netstack is no module, and
/// a joiner runs this machine to reach the mesh before it holds any chain
/// state at all.
pub(crate) const NETSTACK_MODULE_ID: &str = "netstack";

/// how often an unapplied designation says so again: the
/// first miss, then every 60th (roughly a minute of blocks). The counter is
/// the diagnosis — an unconditional line per block would evict the ring.
const RETRY_REPORT_EVERY: u64 = 60;

/// what one tick owes the plane.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// no netstack record, nothing designated, or this process has already
    /// answered this exact designation.
    Nothing,
    /// converge the plane onto this designated component.
    Swap([u8; 32]),
}

/// the code a registry entry designates AT `height`: the scheduled swap once
/// its activation height is reached, else the code already activated.
///
/// GOVERNANCE SCHEDULES A SWAP AT A HEIGHT. The registry's minimum swap lead
/// exists so every node cuts over on the SAME block; acting on the record the
/// moment it is committed would cut each node over at whatever block it first
/// saw it. Below the floor the entry still designates what it activated
/// before, so a node that restarts mid-schedule converges on the code the
/// network runs now rather than waiting. An EMPTY answer designates nothing —
/// an admission that has neither reached its floor nor ever activated.
///
/// The same first read [`modules::code_at`] makes, minus the readiness latch:
/// no validator can ever signal `SwapReady` for a component that is not a
/// `ducktape:module`, and the reachability plane needs no cross-validator
/// synchrony (it contributes no root-hash).
fn designated_code(entry: &modules::ModuleCode, height: u64) -> &[u8] {
    let scheduled = entry
        .pending
        .as_ref()
        .filter(|pending| height >= pending.activation_height);
    match scheduled {
        Some(pending) => &pending.code_hash,
        None => &entry.active_code_hash,
    }
}

/// THE PURE DECISION: the committed registry roster, the committed height, and
/// the designation this process last acted on → this tick's step. Reads
/// nothing, writes nothing.
fn step(modules: &[modules::ModuleCode], height: u64, acted: Option<&[u8; 32]>) -> Step {
    let Some(entry) = modules.iter().find(|m| m.module_id == NETSTACK_MODULE_ID) else {
        return Step::Nothing;
    };
    let Ok(designated) = <[u8; 32]>::try_from(designated_code(entry, height)) else {
        return Step::Nothing; // absent, or a hash no bytes can ever match.
    };
    let already_answered = acted == Some(&designated);
    match already_answered {
        true => Step::Nothing,
        false => Step::Swap(designated),
    }
}

/// Has this designation been ANSWERED — spending its one try? A machine that
/// spoke has decided, whichever way. A swap no plane was running to see
/// decided nothing, and latching it would strand this node on the machine it
/// happens to be on until governance designates something else.
fn spends_the_designation(answer: &SwapAnswer) -> bool {
    match answer {
        SwapAnswer::Swapped(_) | SwapAnswer::Refused(_) => true,
        SwapAnswer::Unattempted(_) => false,
    }
}

/// Select from restored committed state before any protocol event is stepped.
/// Bootstrap bytes are used only when that state designates no netstack code.
pub(crate) async fn startup_backend(
    host: &host::Host,
    height: u64,
    blobs: &noded::blobs::BlobHandle,
) -> Result<reachability::NetstackBackend, String> {
    let bytes = host
        .query(
            host::MODULES_ID,
            &modules::encode_query(&modules::ModulesQuery::ModuleStatus),
        )
        .await
        .map_err(|error| format!("netstack registry: {error}"))?;
    let roster = decode_roster(&bytes)?;
    backend_from_roster(&roster, height, blobs)
}

fn backend_from_roster(
    roster: &[modules::ModuleCode],
    height: u64,
    blobs: &noded::blobs::BlobHandle,
) -> Result<reachability::NetstackBackend, String> {
    let Some(entry) = roster
        .iter()
        .find(|entry| entry.module_id == NETSTACK_MODULE_ID)
    else {
        return crate::reachability_plane::netstack_backend();
    };
    let designated = designated_code(entry, height);
    if designated.is_empty() {
        return crate::reachability_plane::netstack_backend();
    }
    let hash: [u8; 32] = designated
        .try_into()
        .map_err(|_| "netstack designation is not a code hash".to_string())?;
    let bytes = blobs
        .get_chunk(&hash)
        .ok_or_else(|| "designated netstack component is absent".to_string())?;
    Ok(reachability::NetstackBackend::Guest {
        component: artifact_component(&bytes)?,
    })
}

fn artifact_component(bytes: &[u8]) -> Result<Vec<u8>, String> {
    match module_artifact::Artifact::decode(bytes)? {
        module_artifact::Artifact::Module(artifact) => {
            if artifact.index.is_some() {
                return Err("the reachability component has no module index".into());
            }
            Ok(artifact.component)
        }
        module_artifact::Artifact::View(_) => {
            Err("the reachability component is not a view".into())
        }
    }
}

fn decode_roster(bytes: &[u8]) -> Result<Vec<modules::ModuleCode>, String> {
    match modules::decode_reply(bytes).map_err(|error| error.to_string())? {
        modules::ModulesReply::ModuleStatus { modules } => Ok(modules),
        _ => Err("netstack registry returned no module status".into()),
    }
}

struct Answered {
    generation: u64,
    revision: u64,
    hash: [u8; 32],
}

impl Answered {
    fn hash_for(&self, live: &crate::reachability_plane::PlaneExecution) -> Option<&[u8; 32]> {
        let same_execution = self.generation == live.generation && self.revision == live.revision;
        same_execution.then_some(&self.hash)
    }
}

/// Reconcile immediately, then on committed block or actual plane transitions.
/// No timer is needed to repair a plane recreated after promotion or restart.
pub(crate) async fn reconcile(
    label: String,
    metrics: noded::NodeMetrics,
    commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    blobs: noded::blobs::BlobHandle,
    mut blocks: tokio::sync::broadcast::Receiver<noded::BlockWake>,
) {
    let mut execution = crate::reachability_plane::watch_execution();
    let mut acted = None;
    let mut retries = 0;
    loop {
        let live = execution.borrow_and_update().clone();
        reconcile_once(
            &label,
            &metrics,
            &commands,
            &blobs,
            &live,
            &mut acted,
            &mut retries,
        )
        .await;
        tokio::select! {
            wake = blocks.recv() => match wake {
                Ok(_) | Err(RecvError::Lagged(_)) => {},
                Err(RecvError::Closed) => return,
            },
            changed = execution.changed() => {
                if changed.is_err() { return; }
            }
        }
    }
}

async fn reconcile_once(
    label: &str,
    metrics: &noded::NodeMetrics,
    commands: &futures::channel::mpsc::Sender<noded::NodeCommand>,
    blobs: &noded::blobs::BlobHandle,
    live: &crate::reachability_plane::PlaneExecution,
    acted: &mut Option<Answered>,
    retries: &mut u64,
) {
    let Some(roster) = registry_roster(commands).await else {
        return;
    };
    let height = metrics.block_height();
    if crate::reachability_plane::startup_pending(live.generation) {
        crate::reachability_plane::start_pending_netstack(
            live.generation,
            backend_from_roster(&roster, height, blobs),
        );
        return;
    }
    // Starting planes cannot snapshot yet; faults stop the lane and are
    // reported through execution status rather than retried as deployments.
    if live.status.code_hash().is_none() {
        return;
    }
    let answered = acted.as_ref().and_then(|answer| answer.hash_for(live));
    let Step::Swap(designated) = step(&roster, height, answered) else {
        return;
    };
    let Some(bytes) = blobs.get_chunk(&designated) else {
        *retries += 1;
        report_retry(
            label,
            &designated,
            *retries,
            "netstack_code_absent",
            "this node does not hold the designated component's bytes",
        );
        return;
    };
    let component = artifact_component(&bytes);
    let desired_component = component
        .as_ref()
        .ok()
        .map(|bytes| <[u8; 32]>::from(Sha256::digest(bytes)));
    let already_running =
        desired_component.is_some() && desired_component == live.status.code_hash();
    if already_running {
        *acted = Some(Answered {
            generation: live.generation,
            revision: live.revision,
            hash: designated,
        });
        return;
    }
    let answer = match component {
        Ok(component) => {
            crate::reachability_plane::swap_netstack(noded::NetstackSwapRequest::Bytes(component))
                .await
        }
        Err(error) => SwapAnswer::Refused(error),
    };
    crate::reachability_plane::record_swap(metrics, &answer);
    let current = crate::reachability_plane::watch_execution()
        .borrow()
        .clone();
    let answered_execution = match &answer {
        SwapAnswer::Swapped(_) => {
            let same_plane = current.generation == live.generation;
            let runs_designated = current.status.code_hash() == desired_component;
            (same_plane && runs_designated).then_some(&current)
        }
        SwapAnswer::Refused(_) => Some(live),
        SwapAnswer::Unattempted(_) => None,
    };
    if let Some(execution) = answered_execution {
        *acted = Some(Answered {
            generation: execution.generation,
            revision: execution.revision,
            hash: designated,
        });
    }
    *retries = match spends_the_designation(&answer) {
        true => 0,
        false => *retries + 1,
    };
    report_answer(label, &designated, answer, *retries);
}

/// the committed modules registry roster, off the drain's own command lane —
/// the same read the http query surface makes.
async fn registry_roster(
    commands: &futures::channel::mpsc::Sender<noded::NodeCommand>,
) -> Option<Vec<modules::ModuleCode>> {
    let (reply, answer) = futures::channel::oneshot::channel();
    let mut commands = commands.clone();
    commands
        .send(noded::NodeCommand::Query {
            target: host::MODULES_ID.into(),
            req: modules::encode_query(&modules::ModulesQuery::ModuleStatus),
            reply,
        })
        .await
        .ok()?;
    let bytes = answer.await.ok()?.ok()?;
    let Ok(modules::ModulesReply::ModuleStatus { modules }) = modules::decode_reply(&bytes) else {
        return None;
    };
    Some(modules)
}

/// the forever-retry voice: attempt 1, then every [`RETRY_REPORT_EVERY`]th,
/// carrying `attempts`. The counter IS the diagnosis, and a line per block
/// would evict the ring it is evidence in.
fn report_retry(label: &str, designated: &[u8; 32], attempts: u64, reason: &str, detail: &str) {
    let due = attempts == 1 || attempts.is_multiple_of(RETRY_REPORT_EVERY);
    if !due {
        return;
    }
    tracing::warn!(
        target: "ducktape::reachability",
        node = %label,
        reason,
        code_hash = %crate::config::hex_bytes(designated),
        attempts,
        detail = %detail,
        "the netstack component governance designates is not applied here; the next block \
         re-offers it"
    );
}

fn report_answer(label: &str, designated: &[u8; 32], answer: SwapAnswer, retries: u64) {
    let code_hash = crate::config::hex_bytes(designated);
    match answer {
        SwapAnswer::Swapped(backend) => tracing::info!(
            target: "ducktape::reachability",
            node = %label,
            backend = %backend,
            code_hash = %code_hash,
            "the reachability plane is on the netstack component governance designates"
        ),
        SwapAnswer::Refused(reason) => tracing::warn!(
            target: "ducktape::reachability",
            node = %label,
            reason = "netstack_swap_refused",
            code_hash = %code_hash,
            detail = %reason,
            "the plane refused the netstack component governance designates; it keeps \
             running the machine it has and this node will not retry these bytes"
        ),
        SwapAnswer::Unattempted(detail) => {
            report_retry(label, designated, retries, "netstack_plane_absent", &detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_plane_or_running_component_reopens_the_same_designation() {
        let answer = Answered {
            generation: 1,
            revision: 2,
            hash: [7; 32],
        };
        let mut live = crate::reachability_plane::PlaneExecution {
            generation: 1,
            revision: 2,
            status: reachability::BackendStatus::Running { code_hash: [1; 32] },
        };
        let roster = vec![entry(Some(answer.hash), &[])];
        assert_eq!(
            step(&roster, ACTIVATION, answer.hash_for(&live)),
            Step::Nothing
        );
        live.generation += 1;
        assert_eq!(
            step(&roster, ACTIVATION, answer.hash_for(&live)),
            Step::Swap(answer.hash)
        );
        live.generation = 1;
        live.revision += 1;
        assert_eq!(
            step(&roster, ACTIVATION, answer.hash_for(&live)),
            Step::Swap(answer.hash)
        );
    }

    #[test]
    fn restored_selection_uses_committed_bytes_and_refuses_missing_artifacts() {
        let blobs = noded::blobs::BlobHandle::default();
        let component = b"selected component".to_vec();
        let artifact = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
            component: component.clone(),
            index: None,
            view: None,
            lanes: Vec::new(),
        });
        let hash = blobs.put_chunk(artifact.encode());
        let backend = backend_from_roster(&[entry(None, &hash)], 0, &blobs).unwrap();
        assert!(
            matches!(backend, reachability::NetstackBackend::Guest { component: selected, .. } if selected == component)
        );
        let replacement = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
            component: b"replacement".to_vec(),
            index: None,
            view: None,
            lanes: Vec::new(),
        });
        let next_hash = blobs.put_chunk(replacement.encode());
        let scheduled = vec![entry(Some(next_hash), &hash)];
        assert!(
            matches!(backend_from_roster(&scheduled, ACTIVATION - 1, &blobs).unwrap(),
            reachability::NetstackBackend::Guest { component, .. } if component == b"selected component")
        );
        assert!(
            matches!(backend_from_roster(&scheduled, ACTIVATION, &blobs).unwrap(),
            reachability::NetstackBackend::Guest { component, .. } if component == b"replacement")
        );
        assert!(backend_from_roster(&[entry(None, &[9; 32])], 0, &blobs).is_err());
        assert!(backend_from_roster(&[entry(None, &[9; 3])], 0, &blobs).is_err());
    }

    /// governance's own schedule shape: a pending swap AT [`ACTIVATION`], and
    /// whatever code the entry had activated before it.
    const ACTIVATION: u64 = 10;

    fn entry(pending: Option<[u8; 32]>, active: &[u8]) -> modules::ModuleCode {
        modules::ModuleCode {
            module_id: NETSTACK_MODULE_ID.into(),
            kind: modules::Kind::Module,
            active_code_hash: active.to_vec(),
            pending: pending.map(|code_hash| modules::ScheduledSwap {
                name: "netstack-v1".into(),
                activation_height: ACTIVATION,
                code_hash: code_hash.to_vec(),
                readiness: Vec::new(),
                ready_at: None,
            }),
            history: Vec::new(),
        }
    }

    fn other() -> modules::ModuleCode {
        modules::ModuleCode {
            module_id: "kanban".into(),
            kind: modules::Kind::Module,
            active_code_hash: vec![9; 32],
            pending: None,
            history: Vec::new(),
        }
    }

    /// A pending netstack record IS the designation — it can never arm, since
    /// no validator's readiness probe can load a component that is not a
    /// `ducktape:module` — and answering it once is the whole contract: a
    /// refused component is not re-offered every block, a new designation is.
    #[test]
    fn one_swap_per_designation_and_the_pending_record_is_the_designation() {
        let designated = [7; 32];
        let roster = vec![other(), entry(Some(designated), &[])];
        assert_eq!(step(&roster, ACTIVATION, None), Step::Swap(designated));
        assert_eq!(
            step(&roster, ACTIVATION, Some(&designated)),
            Step::Nothing,
            "the same designation is answered exactly once"
        );
        let next = [8; 32];
        assert_eq!(
            step(&[entry(Some(next), &[])], ACTIVATION, Some(&designated)),
            Step::Swap(next),
            "a NEW designation is acted on"
        );
        assert_eq!(
            step(&[other()], ACTIVATION, None),
            Step::Nothing,
            "a network that designates no netstack component swaps nothing"
        );
    }

    /// A SCHEDULED SWAP HAPPENS AT ITS HEIGHT. Governance schedules the cutover
    /// block (the registry's minimum swap lead exists so every node cuts over
    /// on the same one); a node that swapped the moment it saw the record
    /// would cut over at whatever block it first read. Below the floor the
    /// entry designates what it already activated — a node restarting
    /// mid-schedule converges on the code the network runs NOW.
    #[test]
    fn a_scheduled_designation_waits_for_its_activation_height() {
        let scheduled = [7; 32];
        let roster = vec![entry(Some(scheduled), &[])];
        assert_eq!(
            step(&roster, ACTIVATION - 1, None),
            Step::Nothing,
            "below the activation height the schedule designates nothing yet"
        );
        assert_eq!(step(&roster, ACTIVATION, None), Step::Swap(scheduled));
        assert_eq!(step(&roster, ACTIVATION + 1, None), Step::Swap(scheduled));
        assert_eq!(
            step(&roster, ACTIVATION + 1, Some(&scheduled)),
            Step::Nothing,
            "and it is still answered exactly once"
        );

        let running = [3; 32];
        let replacing = vec![entry(Some(scheduled), &running)];
        assert_eq!(
            step(&replacing, ACTIVATION - 1, None),
            Step::Swap(running),
            "before the cutover the entry designates the code already activated"
        );
        assert_eq!(step(&replacing, ACTIVATION, None), Step::Swap(scheduled));
    }

    /// A STALE designation IS REPLACEABLE — the registry lets governance
    /// reschedule a pending that passed its activation height without ever
    /// latching readiness, which is every netstack designation. The
    /// replacement's own height is honoured like any other: below its floor
    /// this node keeps the machine it has, and at the floor the new hash is a
    /// designation this process has not answered.
    #[test]
    fn a_re_designation_waits_for_its_own_height_then_is_acted_on() {
        let spent = [7; 32];
        let next = [8; 32];
        const REDESIGNATION: u64 = 40;
        let mut replaced = entry(Some(next), &[]);
        replaced
            .pending
            .as_mut()
            .expect("pending")
            .activation_height = REDESIGNATION;
        let roster = vec![replaced];
        assert_eq!(
            step(&roster, REDESIGNATION - 1, Some(&spent)),
            Step::Nothing,
            "below the new cutover the replacement designates nothing yet"
        );
        assert_eq!(
            step(&roster, REDESIGNATION, Some(&spent)),
            Step::Swap(next),
            "at its height the replacement is a designation this process has not answered"
        );
    }

    /// A designation is spent by an ANSWER. The plane's refusal is one (the
    /// same bytes buy it again forever); "no plane was running" is not, or a
    /// swap offered in the gap a promotion leaves between two planes would
    /// strand this node on the machine it happens to be on.
    #[test]
    fn only_a_machines_answer_spends_the_designation() {
        assert!(spends_the_designation(&SwapAnswer::Swapped("guest".into())));
        assert!(spends_the_designation(&SwapAnswer::Refused(
            "foreign contract".into()
        )));
        assert!(!spends_the_designation(&SwapAnswer::Unattempted(
            "the reachability plane is not running".into()
        )));
    }

    /// An activated record (the module-code path seating one) designates its
    /// active hash; an entry designating nothing usable — no pending and an
    /// empty or malformed active hash — is never a swap.
    #[test]
    fn an_activated_record_designates_and_an_unusable_one_does_not() {
        let active = [3; 32];
        assert_eq!(
            step(&[entry(None, &active)], 0, None),
            Step::Swap(active),
            "an activation is already in force — there is no height left to wait for"
        );
        assert_eq!(
            step(&[entry(None, &[])], ACTIVATION, None),
            Step::Nothing,
            "an admission with no activation yet designates nothing"
        );
        assert_eq!(
            step(&[entry(None, &[1, 2, 3])], ACTIVATION, None),
            Step::Nothing,
            "a hash no bytes can match is not a swap"
        );
    }
}
