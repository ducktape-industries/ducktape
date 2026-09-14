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
//! once and never retried; only a NEW designation is acted on again. The two
//! non-answers heal on their own and retry on the next block: bytes this node
//! does not hold yet (the code plane's push and the readiness pump's fetch
//! land them), and a swap no plane was running to answer.

use futures::SinkExt as _;
use tokio::sync::broadcast::error::RecvError;

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

/// Watch the registry and converge the plane. One pass per block wake — the
/// node's own event, never a timer — and every pass re-derives from committed
/// state, so a restart, a late join or a dropped wake all heal for free.
pub(crate) async fn reconcile(
    label: String,
    metrics: noded::NodeMetrics,
    commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    blobs: noded::blobs::BlobHandle,
    mut blocks: tokio::sync::broadcast::Receiver<noded::BlockWake>,
) {
    let mut acted: Option<[u8; 32]> = None;
    let mut retries: u64 = 0;
    loop {
        let woken = blocks.recv().await;
        let node_is_alive = match woken {
            // any block wake: the registry may have moved under us.
            Ok(_) => true,
            // wakes were dropped; this read is idempotent, so re-read.
            Err(RecvError::Lagged(_)) => true,
            // the stream hub is gone, and with it the node.
            Err(RecvError::Closed) => false,
        };
        if !node_is_alive {
            return;
        }
        let Some(modules) = registry_roster(&commands).await else {
            continue; // the actor is busy or absent — the next block re-asks.
        };
        // the committed height this node holds — the gauge every applied block
        // sets before the wake that carries it.
        let height = metrics.block_height();
        let Step::Swap(designated) = step(&modules, height, acted.as_ref()) else {
            continue;
        };
        let Some(component) = blobs.get_chunk(&designated) else {
            // the code plane's push and the readiness pump's fetch land them.
            retries += 1;
            report_retry(
                &label,
                &designated,
                retries,
                "netstack_code_absent",
                "this node does not hold the designated component's bytes",
            );
            continue;
        };
        let answer = match module_artifact::Artifact::decode(&component) {
            Ok(module_artifact::Artifact::Module(artifact)) => {
                apply_netstack_artifact(artifact).await
            }
            Ok(module_artifact::Artifact::View(_)) => {
                SwapAnswer::Refused("the reachability component is not a view".into())
            }
            Err(error) => SwapAnswer::Refused(error),
        };
        crate::reachability_plane::record_swap(&metrics, &answer);
        let decided = spends_the_designation(&answer);
        retries = match decided {
            true => 0,
            false => retries + 1,
        };
        if decided {
            acted = Some(designated);
        }
        report_answer(&label, &designated, answer, retries);
    }
}

async fn apply_netstack_artifact(artifact: module_artifact::ModuleArtifact) -> SwapAnswer {
    if artifact.index.is_some() {
        return SwapAnswer::Refused("the reachability component has no module index".into());
    }
    let request = noded::NetstackSwapRequest::Bytes(artifact.component);
    crate::reachability_plane::swap_netstack(request).await
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
        replaced.pending.as_mut().expect("pending").activation_height = REDESIGNATION;
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
