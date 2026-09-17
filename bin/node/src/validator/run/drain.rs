//! Finalized-block drain, checkpoint, and epoch-cutover handling.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::{Recipients, Sender as _};
use commonware_runtime::{Clock as _, IoBuf};
use commonware_utils::ordered::Set;

use consensus::ContentStore;
use recovery::Manifest;
use sdk::Msg;
use tasks::{TaskQuery, TaskReply, decode_task_reply, encode_task_query};

use super::ValidatorRuntime;
use crate::constants::{DRAIN_TICK, NOP_TARGET, VALSET_READ_WARN_EVERY, WORKSPACE_CHECK_INTERVAL};
use crate::drain_actions::{
    CutoverTrigger, EpochActions, StallVoice, capture_breakdown, checkpoint_due, cooldown_until,
    observe_block_beat,
};
use crate::host_reads::{read_valset_members, read_valset_mesh_window, read_valset_residents};
use crate::util::{Presence, fatal, hex, participant_bytes, resident_bytes, unix_ms};

/// One warning when the workspace mark goes missing, then one per this many
/// further checks (`WORKSPACE_CHECK_INTERVAL` apart) for a filesystem that
/// never accepts the rewrite.
const MARK_LOST_WARN_EVERY: u64 = 600;

/// the checkpoint's prune outcome — one discriminant covering floor
/// readiness and the sync retention contract together.
#[derive(Debug, PartialEq, Eq)]
enum PruneAction {
    /// the finalization floor hasn't passed this checkpoint yet: nothing to
    /// prune.
    NotDue,
    /// this prune would drop frames a syncer we are serving still needs: hold
    /// it (see `prune_cuts_served_sync`).
    HeldForSync,
    /// nothing in flight needs what this prune drops.
    Prune,
}

/// decide a checkpoint's prune action from the two governing facts. pure —
/// the drain loop performs the prune for whichever action this returns, so
/// the decision itself is testable without a journal or a lease.
fn decide_prune_action(floor_passed: bool, cuts_served_sync: bool) -> PruneAction {
    match (floor_passed, cuts_served_sync) {
        (false, _) => PruneAction::NotDue,
        (true, true) => PruneAction::HeldForSync,
        (true, false) => PruneAction::Prune,
    }
}

/// whether pruning the journal below the previous checkpoint at height
/// `anchor` would drop frames a syncer this node is serving still needs
/// (`needed_from`, from `sync::serve::SyncRetention::floor`).
///
/// THE ANTI-TREADMILL CONTRACT. A prune retains from `anchor + 1`; a syncer
/// working from `needed_from` asks for the frames after it. Pruning past that
/// height answers its catch-up with `RangePruned`, it re-bootstraps at a newer
/// boundary, and a chain that prunes faster than one bootstrap takes never
/// lets it converge (observed on a 100 ms chain with a 4-block checkpoint
/// cadence: boundary 1752 → 2117 → 2360 → … forever).
///
/// The hold is bounded by the syncer's OWN PROGRESS, never by a count of
/// checkpoints: every chunk it pulls and every frame batch it folds restates
/// the height it works from, so retention advances with it and lapses when it
/// stops asking. A count would be denominated in blocks, and a busy chain
/// burns any count of checkpoints in seconds.
fn prune_cuts_served_sync(anchor: Option<u64>, needed_from: Option<u64>) -> bool {
    match (anchor, needed_from) {
        (Some(anchor), Some(needed_from)) => anchor > needed_from,
        _ => false,
    }
}

/// how long a validator may seal NOTHING before the halt detector calls it a
/// stall: 30 idle beats. an idle chain beats one nop block per `block_time`,
/// so a window this wide cannot fire on ordinary jitter (a checkpoint, a slow
/// disk, a view or two nullified while a peer reconnects) and still names a
/// wedge in seconds rather than minutes.
fn stall_window(cadence: consensus::Cadence) -> std::time::Duration {
    cadence.block_time * 30
}
use crate::validator::code_announce::{CodeVerdict, Role};
use crate::{join_gate, relay};
use noded::projection::{BlockProjection, project_block};

impl ValidatorRuntime<'_> {
    /// one drain turn: the pass over delivered finalizations, then the
    /// `/v1/status` publish — the boundary a pass settles must be visible on
    /// the cell the moment the turn ends, and `publish_status` owns the
    /// (throttled) operations refresh, so the pass itself stays free of it.
    pub(super) async fn on_drain(&mut self) {
        self.guard_workspace();
        self.drain_pass().await;
        self.publish_status().await;
    }

    /// FAIL-STOP on a workspace deleted underneath a running node.
    ///
    /// Deleting it (an `rm -rf` of the ducktape home, a disk unmount) leaves
    /// consensus writing into a tree that no longer exists, and the first
    /// journal prune that misses a blob PANICS inside the consensus voter —
    /// a task panic in a dependency, with no `FATAL:` line, no reason token,
    /// and no way for the app's log reader to classify the death. This is the
    /// last place OUR code stands between the deletion and that panic, so it
    /// takes the stop: one stat per `WORKSPACE_CHECK_INTERVAL`, and the node
    /// exits through the same marker as every other fatal path.
    ///
    /// It exits WITHOUT a checkpoint on purpose: a checkpoint writes, and a
    /// write re-creates the very directory tree that was just deleted —
    /// leaving a half-born workspace behind as the node's last act.
    fn guard_workspace(&mut self) {
        let due = self.context.current() >= self.next_workspace_check;
        if !due {
            return;
        }
        self.next_workspace_check = self.context.current() + WORKSPACE_CHECK_INTERVAL;
        let Some(mark) = self.workspace_mark else {
            return;
        };
        match mark.presence(&self.workspace) {
            Presence::Intact => self.workspace_mark_lost_checks = 0,
            Presence::MarkLost => self.report_lost_workspace_mark(),
            Presence::Vanished => fatal!(
                self.label,
                reason = "storage_vanished",
                "the workspace directory {} was deleted underneath this node — halting without a checkpoint",
                self.workspace.display()
            ),
        }
    }

    /// The workspace is the one we booted on, but its mark file is gone or
    /// short. Never fatal: put the token back, and say so — paced, because a
    /// filesystem that refuses the write refuses it again every second, and an
    /// unconditional warn in a forever-retry loop evicts the ring it is
    /// supposed to explain.
    fn report_lost_workspace_mark(&mut self) {
        let Some(mark) = self.workspace_mark else {
            return;
        };
        self.workspace_mark_lost_checks += 1;
        let attempts = self.workspace_mark_lost_checks;
        let restored = mark.restore(&self.workspace);
        let speak = attempts == 1 || attempts.is_multiple_of(MARK_LOST_WARN_EVERY);
        if !speak {
            return;
        }
        let reason = match restored {
            true => "workspace_mark_restored",
            false => "workspace_mark_unwritable",
        };
        tracing::warn!(
            target: "ducktape::node",
            node = %self.label,
            reason,
            attempts,
            "the workspace mark under {} is missing — the directory is still the one this node booted on, so this is not a deletion",
            self.workspace.display()
        );
    }

    async fn drain_pass(&mut self) {
        let Self {
            context,
            node,
            orchestrator,
            epoch_spawner,
            last_cert_height,
            latest_floor,
            mesh_oracle,
            mesh_window,
            mesh_book,
            gateway_book,
            media_peers,
            blob_peers,
            reach_cmd,
            relay_tx,
            gate_outcomes,
            join_requests,
            next_seq,
            prev_ckpt,
            signer,
            label,
            checkpoint_blocks,
            sync_retention,
            stream_hub,
            index,
            blobs,
            metrics,
            applied,
            pending_submits,
            pending_rpc_submits,
            pending_relays,
            pending_gates,
            gating,
            validator_relay,
            last_published,
            blocks_since_checkpoint,
            checkpoint_not_before,
            last_written_root,
            last_reach_view,
            pending_retarget,
            next_drain,
            delivery_wake_tx,
            real_work_parked,
            last_seal,
            stall_windows,
            heartbeat_disabled,
            cadence,
            valset_read_failed_checks,
            ..
        } = self;
        let context = *context;
        let cadence = *cadence;
        let checkpoint_blocks = *checkpoint_blocks;

        *next_drain = context.current() + DRAIN_TICK;
        // FAIL-STOP: a drain error is a node-local block-boundary
        // fault — this node's state is indeterminate relative to its
        // peers, so applying even one more finalized op could
        // silently fork it. exit loudly; an operator (or supervisor)
        // restarts the node, which then re-joins via state sync.
        let drained_count = match node.drain_delivered().await {
            Ok(n) => n,
            // STALLED, not faulted: the block at that height designates module
            // code this node cannot resolve yet. the frame is still at the
            // front of the drain queue and the blocks ahead of it settled — so
            // keep the tick, keep the state, and let the next drain retry
            // (which is what re-runs the fetch). halting here would stop a
            // chain that was only waiting for bytes; the node narrates the
            // stall with its attempt count.
            Err(node::Error::CodeStalled { applied, .. }) => applied,
            Err(e) => {
                fatal!(label, "{e} — halting");
            }
        };
        *applied += drained_count;
        // NO tip-seal sync here. the seal fsyncs where it is written
        // (`recovery`'s `BlockSink::seal`), so the tip is durable the moment
        // it is sealed — idle chain or busy. an idle-transition sync used to
        // stand here to close the same window from the far side; keeping both
        // would be a second path to one guarantee.
        // resolve held app-surface submits against what this
        // drain finished with; every disposition is deterministic,
        // so the reply faithfully reports the op's consensus fate.
        let drained = node.take_drained();
        // sealed = journaled: one seal per BLOCK (height), whatever a
        // batch's member count. count DISTINCT sealed heights so the
        // checkpoint cadence stays per-block; applied and rejected
        // members both seal, discarded frames never sealed a height.
        let sealed_heights = drained
            .iter()
            .filter(|d| d.disposition != node::Disposition::Discarded)
            .map(|d| d.height)
            .collect::<std::collections::BTreeSet<u64>>()
            .len() as u64;
        *blocks_since_checkpoint += sealed_heights;
        // THE HALT DETECTOR. every way this loop can stop producing blocks is
        // silent by construction: a release gate awaiting bytes delivers
        // nothing, a pending FIFO that never empties makes the idle beat
        // restamp forever, and neither logs a thing. so the ABSENCE of blocks
        // is the event — one warn per stall window, carrying the state that
        // names which wedge it is (#1766). the heartbeat is what guarantees a
        // block per beat, so a node with it disabled (`make dev`) has no floor
        // to measure against and is not watched.
        let beat = observe_block_beat(
            last_seal,
            stall_windows,
            context.current(),
            sealed_heights > 0,
            stall_window(cadence),
            *heartbeat_disabled,
        );
        // the gauge every turn, not only on a report: a dashboard's whole job
        // is to show the silence RISING, and a value that only appears once a
        // window has elapsed is a value that appears after the outage matters.
        metrics.record_block_beat(beat.stalled_for);
        macro_rules! block_beat_stalled {
            ($emit:ident, $message:literal) => {
                tracing::$emit!(
                    target: "ducktape::consensus",
                    node = %label,
                    // `event`, not `reason`: this is an operational-contract
                    // name the status projection and the metrics key on, not a
                    // token for something we refused (AGENTS.md, Logging).
                    event = "block_beat_stalled",
                    windows = beat.windows,
                    stalled_ms = beat.stalled_for.as_millis() as u64,
                    epoch = orchestrator.epoch(),
                    validators = orchestrator.current_members().len(),
                    height = node.finalized().map_or(0, |f| f.height),
                    pending_ops = node.pending_batch_len(),
                    orderer_pending = node.orderer().pending_len(),
                    gate_awaiting = node.orderer().min_unreleased_view().is_some(),
                    gate_min_view = node.orderer().min_unreleased_view().unwrap_or(0),
                    $message
                )
            };
        }
        match beat.voice {
            StallVoice::Quiet => {}
            StallVoice::Warn => block_beat_stalled!(
                warn,
                "no block sealed for a full stall window — the chain is not beating"
            ),
            StallVoice::Error => block_beat_stalled!(
                error,
                "the chain has been silent past the point it recovers on its own"
            ),
        }
        // The orderer-independent projection keeps member/System order,
        // explorer rows, and discard handling identical to the replica.
        let projections = project_block(&drained, node.take_system_dispatches(), blobs);
        // THE PASS PUBLISHES ONE TIP FRAME FOR N BLOCKS, so the wake is the OR
        // over all of them and never the tip block's alone. A pass of
        // [chat@10, nop@11] publishes h=11, and the 1s nop filler makes that the
        // ORDINARY busy shape — taking the tip block's answer would strand the
        // rows at h=10 until some later block happened to feed chat again.
        let pass_wake = match projections.iter().any(|p| !p.dispatches.is_empty()) {
            true => noded::BlockWake::IndexChanged,
            false => noded::BlockWake::TipOnly,
        };
        for projection in projections {
            // NOTHING SEALED AT THIS HEIGHT — every frame in the batch was
            // discarded by the cutover ceiling. There is no block here to
            // project, and folding one would advance every module's watermark
            // onto a height the NEW epoch is about to use (see
            // `BlockProjection::sealed`).
            if !projection.sealed() {
                continue;
            }
            let BlockProjection {
                height,
                dispatches,
                record,
                applied,
                latency_us,
                applied_ops,
                rejected_ops,
                ..
            } = projection;
            // one block per height: an APPLIED block records fully
            // (count, this node's summed apply latency, per-module
            // dispatch counters); an all-rejected block (the idle nop
            // lands here) only follows the height gauge. ops_total
            // counts the aggregated member ops.
            if applied {
                metrics.record_block(height, latency_us, &dispatches);
            } else {
                metrics.record_height(height);
            }
            metrics.record_op_outcomes(applied_ops, rejected_ops);
            // this lane's agreed clock IS the height: the drain stamps
            // BlockContext { consensus_time: height } for every block. the
            // shared index-fold epilogue owns the STALE-index error log.
            noded::projection::apply_block_to_index(
                index,
                height,
                height,
                record,
                &dispatches,
                node.host(),
            );
        }
        for d in drained {
            // a DISCARD is not this hold's outcome: the cutover
            // carries the frame into the new epoch under the SAME
            // FrameId, so the hold stays open until the carried
            // frame finalizes there (or SUBMIT_HOLD expires into
            // the truthful re-query reply).
            if d.disposition == node::Disposition::Discarded {
                continue;
            }
            // resolve a HELD JOIN GATE: the joiner's
            // outcome was held against this Redeem frame — now the drain knows
            // its consensus fate. Applied ⇒ the AUTHORITATIVE Admitted at the
            // committed height (carrying the coord cap); Rejected ⇒ map the
            // module reason to a code + terminal bit (chiefly a spent-nonce
            // race the pre-filter missed). the outcome goes into the shared
            // `gate_outcomes` map, where the intro doorbell answers the
            // joiner's next tunnel retransmit — this loop owns no doorbell
            // socket. a gate frame is EXCLUSIVELY a gate, so resolve and move
            // on.
            if let Some(gate) = pending_gates.remove(&d.id) {
                gating.remove(&gate.joiner);
                let reply = match d.disposition {
                    node::Disposition::Applied => join_gate::IntroReply::Admitted {
                        height: d.height,
                        cap: gate.cap,
                    },
                    node::Disposition::Rejected => {
                        // settle-race guard: this member's Redeem lost to a
                        // SIBLING member's grant for the same joiner (a slow
                        // settle swept us to Busy, the joiner failed over, and
                        // both Redeems batched — governance answers "already
                        // redeemed" to the loser). the joiner IS admitted; a
                        // terminal Spent here would `exit(1)` a granted join.
                        // if the joiner now holds resident standing, answer
                        // Admitted, not Spent.
                        let admitted = read_valset_residents(node.host())
                            .await
                            .iter()
                            .any(|r| r.as_slice() == gate.joiner.as_slice());
                        if admitted {
                            join_gate::IntroReply::Admitted {
                                height: d.height,
                                cap: gate.cap,
                            }
                        } else {
                            let (code, terminal) =
                                join_gate::redeem_reject_outcome(d.reason.as_deref());
                            join_gate::IntroReply::Rejected {
                                code,
                                detail: d.reason.clone().unwrap_or_else(|| {
                                    "invite redemption rejected in consensus".into()
                                }),
                                terminal,
                            }
                        }
                    }
                    node::Disposition::Discarded => unreachable!("filtered at the loop top"),
                };
                // on a GRANT, track the widened mesh window BEFORE the
                // Admitted settles (blessed decision #1): the joiner's next
                // act is dialing statesync, so its real key must be tracked
                // first. the grant advanced the membership GENERATION at its
                // commit block, so this lands on a fresh monotonic index —
                // the old same-epoch re-track was silently warn-dropped by
                // commonware ("peer set already exists") and the joiner
                // actually waited out the cutover. every validator resolves
                // this same Applied block in its own drain, so the widened
                // window converges within a beat.
                if let join_gate::IntroReply::Admitted { .. } = &reply {
                    let window = read_valset_mesh_window(node.host()).await;
                    mesh_window.track_new(mesh_oracle, mesh_book, &window);
                }
                super::settle_gate(gate_outcomes, gate.joiner, reply, context.current());
                continue;
            }
            // resolve a relayed hold FIRST: a relayed frame has no
            // local pending_submits entry, so this must precede the
            // `else { continue }` below or the wire Reply is lost.
            if let Some((peers, _)) = pending_relays.remove(&d.id) {
                let outcome = match d.disposition {
                    node::Disposition::Applied => relay::RelayOutcome::Applied {
                        height: d.height,
                        root_hash: hex(&d.root_hash),
                    },
                    node::Disposition::Rejected => relay::RelayOutcome::Rejected {
                        // carry the module's VERBATIM reason (node-
                        // local observability off the DrainedFrame)
                        // so the resident forwards it to its caller
                        // — the duckfs-client engine keys on the
                        // "files: conflict:" prefix. generic wording
                        // only when the drain captured no reason.
                        detail: d.reason.clone().unwrap_or_else(|| {
                            "op finalized but rejected (deterministic no-op)".into()
                        }),
                    },
                    node::Disposition::Discarded => unreachable!("filtered at the loop top"),
                };
                let msg = relay::RelayMsg::Reply {
                    frame_id: d.id,
                    outcome,
                };
                // every courier that relayed this frame gets the SAME outcome.
                for peer in peers {
                    let _ = relay_tx.send(
                        Recipients::One(peer),
                        IoBuf::from(relay::encode_msg(&msg)),
                        false,
                    );
                }
            }
            // BEFORE the pending_submits lookup, deliberately. An op rejected in
            // consensus produced no record ANYWHERE: the submitter's own log says
            // SUCCESS (the submit was accepted) while the state machine says NO.
            //
            // and the internal submits — oracle results, upgrade readiness,
            // code-ready signals — are fire-and-forget and never enter
            // `pending_submits` at all, so the `continue` below swallows their
            // rejection whole. The capability announce used to be the worst case
            // of that and no longer travels this lane at all (it is a settling
            // `/v1/submit` from outside this loop now); the rest are still
            // unrouted, which is its own item.
            let module = d.op.as_ref().map_or("system", |op| op.target.as_str());
            // the idle-chain NOP filler is rejected BY DESIGN — it targets a module
            // that deliberately does not exist. warning on it would fire every block
            // forever on an idle chain, evicting the whole 4096-line ring in ~68
            // minutes and drowning the very evidence someone came to read.
            let rejected = d.disposition == node::Disposition::Rejected;
            let is_idle_nop = module == NOP_TARGET;
            if rejected && !is_idle_nop {
                tracing::warn!(
                    target: "ducktape::submit",
                    node = %label,
                    frame = %noded::hex_bytes(&d.id),
                    height = d.height,
                    module,
                    reason = %d.reason.as_deref().unwrap_or("deterministic_no_op"),
                    "op rejected in consensus"
                );
            }
            // the rpc lane's parked callers, answered from the same
            // disposition and at the same moment as the http lane's (#2533).
            // A refusal reaches the person who typed the verb, carrying the
            // module's own reason token — the one the warn above logs.
            if let Some((rpc_replies, _)) = pending_rpc_submits.remove(&d.id) {
                let settled =
                    crate::drain_actions::settled_submit(rejected, d.reason.as_deref());
                for rpc_reply in rpc_replies {
                    let line = match &settled {
                        Ok(()) => crate::rpc::RpcReply::ok(),
                        Err(reason) => crate::rpc::RpcReply::err(reason.clone()),
                    };
                    let _ = rpc_reply.send(line);
                }
            }
            let Some((replies, _)) = pending_submits.remove(&d.id) else {
                continue;
            };
            let outcome = match d.disposition {
                node::Disposition::Applied => Ok(noded::BlockSummary {
                    height: d.height,
                    // the PER-BLOCK boundary this frame settled at
                    // (not the end-of-drain hash — a drain can
                    // apply several blocks).
                    root_hash: hex(&d.root_hash),
                }),
                // the two rejections were one string and are now two
                // tokens, which is the difference between "the module
                // said no" and "nothing was there to apply".
                node::Disposition::Rejected => Err(match d.reason.clone() {
                    // the module's VERBATIM reason when the drain
                    // captured one (duckfs-client keys on the
                    // "files: conflict:" prefix).
                    Some(said) => noded::Refused::new("module", said),
                    None => noded::Refused::new(
                        "deterministic_no_op",
                        "op finalized but rejected (deterministic no-op)",
                    ),
                }),
                // unreachable — filtered at the loop top — but
                // stay total rather than panic.
                node::Disposition::Discarded => continue,
            };
            // every caller that submitted this frame gets the SAME outcome:
            // one FrameId is one consensus unit, however many asked for it.
            for reply in replies {
                let _ = reply.send(outcome.clone());
            }
        }
        validator_relay.expire(context.current(), relay_tx);
        // expire holds the mesh never finalized in time. the op may
        // still land later — clients re-query on block events.
        if !pending_submits.is_empty() {
            let now = context.current();
            let expired: Vec<node::FrameId> = pending_submits
                .iter()
                .filter(|(_, (_, deadline))| *deadline <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                let Some((replies, _)) = pending_submits.remove(&k) else {
                    continue;
                };
                for reply in replies {
                    let _ = reply.send(Err(noded::Refused::new(
                        "finalization_timeout",
                        "timed out awaiting finalization — re-query on the next block",
                    )));
                }
            }
        }
        // the same contract for the rpc lane's parked callers: now that a
        // refusal comes back by itself, a timeout here means ONLY that the op
        // has not finalized yet — which is what the sentence has to say (#2533).
        if !pending_rpc_submits.is_empty() {
            let now = context.current();
            let expired: Vec<node::FrameId> = pending_rpc_submits
                .iter()
                .filter(|(_, (_, deadline))| *deadline <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                let Some((replies, _)) = pending_rpc_submits.remove(&k) else {
                    continue;
                };
                for reply in replies {
                    let _ = reply.send(crate::rpc::RpcReply::err(
                        "finalization_timeout: submitted, not finalized yet — re-query on the \
                         next block"
                            .to_string(),
                    ));
                }
            }
        }
        // the same expiry contract for relayed holds: the mesh never
        // finalized in time, so answer the resident truthfully — the
        // op may still land, it re-queries on the next block.
        if !pending_relays.is_empty() {
            let now = context.current();
            let expired: Vec<node::FrameId> = pending_relays
                .iter()
                .filter(|(_, (_, deadline))| *deadline <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                let Some((peers, _)) = pending_relays.remove(&k) else {
                    continue;
                };
                let msg = relay::RelayMsg::Reply {
                    frame_id: k,
                    outcome: relay::RelayOutcome::Refused {
                        detail: "timed out awaiting finalization — re-query on the next block"
                            .into(),
                    },
                };
                for peer in peers {
                    let _ = relay_tx.send(
                        Recipients::One(peer),
                        IoBuf::from(relay::encode_msg(&msg)),
                        false,
                    );
                }
            }
        }
        // held join gates that never settled within GATE_SETTLE_TIMEOUT: write
        // Busy (NON-terminal) into the outcome map so the joiner's next
        // retransmit reads it and fails over to another member rather than
        // exiting. the Redeem may still land later — a re-forward then hits
        // the V9 idempotent Admitted.
        if !pending_gates.is_empty() {
            let now = context.current();
            let expired: Vec<node::FrameId> = pending_gates
                .iter()
                .filter(|(_, g)| g.deadline <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                if let Some(gate) = pending_gates.remove(&k) {
                    gating.remove(&gate.joiner);
                    super::settle_gate(
                        gate_outcomes,
                        gate.joiner,
                        join_gate::IntroReply::Rejected {
                            code: join_gate::RejectCode::Busy,
                            detail: "the gate could not settle in time — trying another member"
                                .into(),
                            terminal: false,
                        },
                        now,
                    );
                }
            }
        }
        // sweep gate outcomes settled more than a join window ago — on the
        // SAME beat as the held-gate expiry just above, no new timer. keyed
        // on `INVITE_JOIN_WINDOW_MS` (the same clock the invite-peer tunnel
        // table itself ages uncovered entries out by, `merge_invite_layer` in
        // netstack-machine): a joiner has that long to retransmit before its
        // outcome — `Admitted` included — is dead weight. Expiring `Admitted`
        // is safe: a joiner that reappears later just re-runs the gate, and
        // `on_gate_forward`'s V9 arm answers Admitted again for free (a
        // committed-state read, no consensus round).
        crate::reachability_plane::sweep_gate_outcomes(
            &mut gate_outcomes.lock().expect("gate outcomes lock"),
            context.current(),
            std::time::Duration::from_millis(reachability::INVITE_JOIN_WINDOW_MS),
        );
        // same reasoning, same beat, for the join-request map: a joiner that
        // stops retransmitting (a rejected Redeem, or it just gave up) would
        // otherwise sit there forever — the only other prune is the read-time
        // retain in `on_rpc` for a joiner that GAINED standing.
        crate::rpc::sweep_join_requests(
            join_requests,
            unix_ms(),
            reachability::INVITE_JOIN_WINDOW_MS,
        );
        // publish each newly-applied boundary to ws subscribers
        // (send only errs when nobody is subscribed — fine). the
        // drain loop above already folded each block into the
        // metrics series; this tip seam carries the ws block
        // summary only — it fires once per drain.
        if let Some(f) = node.finalized()
            && *last_published != Some(f.height)
        {
            stream_hub.publish_block(f.height, hex(&f.root_hash), pass_wake);
            *last_published = Some(f.height);
        }

        // persist the finalization floor for the newest certificate
        // whose view has fully drained. read the certificate FIRST,
        // the release point second: releases happen only on this
        // thread, so a certificate strictly below every slot still
        // pending at the later read is fully applied — a floor ahead
        // of app state would suppress replay of finalized ops a
        // restart still needs. matched to the SEALED tip view (not
        // just the newest certificate) and deliberately NOT gated on
        // a momentarily empty inbox: on a busy chain a fresh
        // finalization is (nearly) always in flight, and the empty-
        // inbox gate starved — the floor stopped tracking the tip and
        // the statesync boundary serve (floor == tip, exactly)
        // refused every joiner for as long as the load lasted.
        if let Some(tip_view) = node.finalized_view()
            && let Some((view, cert)) = node.orderer().finalization_at_or_below(tip_view)
            && view != 0
            && node
                .orderer()
                .min_unreleased_view()
                .is_none_or(|pending| pending > view)
        {
            let height = orchestrator.app_height(view);
            if last_cert_height.is_none_or(|h| height > h) {
                let fc = recovery::FloorCert {
                    epoch: orchestrator.epoch(),
                    height,
                    cert,
                };
                match node.sink_mut().write_floor_cert(&fc).await {
                    Ok(()) => {
                        *last_cert_height = Some(height);
                        *latest_floor = Some(fc);
                    }
                    Err(e) => tracing::warn!(
                        target: "ducktape::recovery",
                        node = %label,
                        height,
                        error = %e,
                        "floor cert write failed; retrying"
                    ),
                }
            }
        }

        // the VALSET ORCHESTRATION step: observe the finalized
        // membership projection; a change schedules a deterministic
        // cutover (arming the discard ceiling), and crossing the
        // cutover view tears the engine down and respawns it over
        // the set read AT the boundary. the observation barrier
        // guarantees this tick's last view IS the changing block's
        // view when membership moved.
        if let Some(engine_view) = node.last_engine_view() {
            // tick the reachability plane's freshness clock.
            // engine views are EPOCH-LOCAL (they reset at every
            // cutover), so convert to the absolute app-height
            // clock (`epoch_base + view`) — the regime the boot
            // Retarget's `view_base` put the plane's advert and
            // handshake expiries in.
            if let Some(cmd) = &reach_cmd {
                let absolute_view = orchestrator.app_height(engine_view);
                if last_reach_view.is_none_or(|v| v < absolute_view) {
                    // NON-BLOCKING: the plane is not consensus. a
                    // full command queue (a wedged or slow plane)
                    // sheds this tick — the next drain beat carries
                    // a fresher one — instead of stalling the loop
                    // behind an actor that may never drain.
                    let _ =
                        cmd.try_send(reachability::ReachabilityCommand::ViewTick(absolute_view));
                    *last_reach_view = Some(absolute_view);
                }
                // flush a staged cutover Retarget (see
                // `pending_retarget`) — MUST eventually land, so
                // it retries every beat rather than being shed.
                if let Some(event) = pending_retarget.take()
                    && let Err(tokio::sync::mpsc::error::TrySendError::Full(
                        reachability::ReachabilityCommand::Retarget(event),
                    )) = cmd.try_send(reachability::ReachabilityCommand::Retarget(event))
                {
                    *pending_retarget = Some(event);
                }
            }
            // sync the mesh window at the same frozen read point: any
            // committed membership change — a sibling member's grant, a
            // governance leave/revoke — widens or narrows the mesh NOW, at
            // its generation index; the epoch cutover below stays an
            // engine/channel concern. monotonic bookkeeping makes the
            // no-change case a silent no-op.
            let committed_window = read_valset_mesh_window(node.host()).await;
            mesh_window.track_new(mesh_oracle, mesh_book, &committed_window);
            match read_valset_members(node.host()).await {
                Err(e) => {
                    // a failed read is NOT an observation: feeding it to
                    // `observe_members` as an empty set would arm a bogus
                    // cutover no other validator armed (#1820). skip the
                    // cutover step this tick and retry next block.
                    *valset_read_failed_checks += 1;
                    let attempts = *valset_read_failed_checks;
                    let speak = attempts == 1 || attempts.is_multiple_of(VALSET_READ_WARN_EVERY);
                    if speak {
                        tracing::warn!(
                            target: "ducktape::consensus",
                            node = %label,
                            reason = "valset_read_failed",
                            attempts,
                            error = %e,
                            "committed valset read failed; skipping the cutover step this tick"
                        );
                    }
                }
                Ok(members_raw) if members_raw.is_empty() => {
                    // a successful read that comes back EMPTY is the
                    // impossible state the boot path already fatals on (see
                    // `wiring.rs`'s empty mesh-window check) — never a
                    // demotion, and never fed to `observe_members` either.
                    fatal!(
                        label,
                        reason = "valset_read_empty",
                        "committed valset query returned an empty set — an \
                         unreadable valset must never read as an observation \
                         of an empty one"
                    );
                }
                Ok(members_raw) => {
                    *valset_read_failed_checks = 0;
                    let observed: Vec<ed25519::PublicKey> = members_raw
                        .iter()
                        .filter_map(|key| ed25519::PublicKey::decode(key.as_slice()).ok())
                        .collect();
                    // the RESIDENT projection, read at the same frozen
                    // point: a grant/revoke arms the same single cutover
                    // slot (mesh admission is epoch-scoped).
                    let residents_raw = read_valset_residents(node.host()).await;
                    let observed_residents: Vec<ed25519::PublicKey> = residents_raw
                        .iter()
                        .filter_map(|key| ed25519::PublicKey::decode(key.as_slice()).ok())
                        .collect();
                    let mut actions =
                        EpochActions::new(orchestrator, engine_view, observed, observed_residents);
                    if let Some(CutoverTrigger::Membership(cutover)) = actions.observe_members() {
                        tracing::info!(
                            target: "ducktape::consensus",
                            node = %label,
                            observed_view = cutover.observed_view(),
                            next_epoch = cutover.next_epoch(),
                            cutover_view = cutover.cutover_view(),
                            "membership change observed"
                        );
                        node.set_view_ceiling(cutover.cutover_view());
                    }
                    if let Some(plan) = actions.respawn() {
                        let members = plan.valset().consensus_members();
                        let member_bytes: Vec<Vec<u8>> =
                            members.iter().map(|k| k.as_ref().to_vec()).collect();
                        let plan_residents: Vec<ed25519::PublicKey> = plan
                            .valset()
                            .transport_members()
                            .difference(members)
                            .cloned()
                            .collect();
                        let plan_resident_bytes: Vec<Vec<u8>> =
                            plan_residents.iter().map(|k| k.as_ref().to_vec()).collect();
                        // no mesh track here: the TRANSPORT union was already
                        // tracked at its GENERATION index by the window sync
                        // above, the moment the membership change committed —
                        // CUTOVER_DELAY views before this cutover. the epoch
                        // plane below (books, channels, engine) follows now.
                        // the gateway plane serves (and admits) exactly
                        // who the mesh tracks — follow the cutover.
                        if let Some(book) = &gateway_book {
                            book.peers()
                                .set_peers(plan.valset().transport_members().iter());
                        }
                        // the media planes authenticate inbound by the same
                        // tracked set — follow the re-track too, so a
                        // just-added member's huddle media is admitted.
                        if let Some(peers) = &media_peers {
                            peers.set_peers(plan.valset().transport_members().iter());
                        }
                        // the blob code lane's peer book follows the same
                        // cutover — a fetch after a membership change asks
                        // the members that actually exist.
                        *blob_peers.write().expect("blob peers lock") =
                            plan.valset().transport_members().iter().cloned().collect();
                        // the reachability plane retunnels for the new
                        // member set the moment transport admits it —
                        // with the epoch's resident tier as the pre-warm
                        // standbys, so a registered joiner's tunnels
                        // assemble ahead of its activation cutover.
                        // cutover_app_height IS the new epoch's absolute
                        // view at engine view 0 — the raw engine_view
                        // here would be epoch-local, a different clock
                        // than the ViewTicks above and the boot
                        // Retarget's view_base.
                        if reach_cmd.is_some() {
                            // STAGED, not sent inline: the flush below
                            // (every drain beat) try_sends it, so a plane
                            // whose queue is full delays retunneling by
                            // beats — it can never stall the cutover or
                            // the loop.
                            *pending_retarget = Some(reachability::MeshEpochEvent {
                                epoch: plan.epoch(),
                                members: members.iter().cloned().collect(),
                                standbys: plan_residents.clone(),
                                current_view: plan.cutover_app_height(),
                            });
                        }
                        if !members.contains(&signer.public_key()) {
                            tracing::info!(
                                target: "ducktape::consensus",
                                node = %label,
                                epoch = plan.epoch(),
                                "demoted from the validator set; halting"
                            );
                            std::process::exit(0);
                        }
                        let participants: Set<ed25519::PublicKey> =
                            Set::try_from(members.iter().cloned().collect::<Vec<_>>())
                                .expect("orchestrator membership has no duplicates");
                        // a fresh epoch: new store (pins of the torn-down
                        // epoch die with it), genesis floor.
                        let orderer = epoch_spawner
                            .spawn(plan.epoch(), participants, ContentStore::new(), None)
                            .await;
                        // the fresh engine must keep draining event-driven —
                        // re-install the finalization delivery wake on its inbox.
                        orderer.set_delivery_wake(delivery_wake_tx.clone());
                        match node
                            .cutover(
                                orderer,
                                plan.epoch(),
                                plan.cutover_app_height(),
                                &member_bytes,
                                &plan_resident_bytes,
                            )
                            .await
                        {
                            // the accept contract crossing the boundary:
                            // every locally-accepted op the old epoch
                            // never resolved was re-proposed into the
                            // new engine.
                            Ok(carried) if carried > 0 => {
                                // carried ops are real parked work in the fresh
                                // engine — keep the leader-nudge escort walking them.
                                *real_work_parked = true;
                                tracing::info!(
                                    target: "ducktape::consensus",
                                    node = %label,
                                    carried,
                                    epoch = plan.epoch(),
                                    "accepted ops carried across the cutover"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                fatal!(label, "{e} — halting");
                            }
                        }
                        // checkpoint IMMEDIATELY: the manifest must record
                        // the new epoch's participant set (the journal's
                        // cutover record alone covers only the crash
                        // window until this write lands).
                        let pos = node.sink_mut().oplog_pos().await;
                        let captured = Manifest::capture(
                            node.host(),
                            node.finalized().map(|f| f.height),
                            orchestrator.epoch(),
                            orchestrator.epoch_base(),
                            participant_bytes(orchestrator),
                            resident_bytes(orchestrator),
                            None,
                            pos,
                            *next_seq,
                        )
                        // the replay guard rides every checkpoint (see
                        // `recovery::Manifest::applied_frames`).
                        .map(|m| m.with_replay_window(node.replay_window()));
                        let post_cutover_started = context.current();
                        match captured {
                            Ok(m) => match node.sink_mut().write_manifest(&m).await {
                                Ok(()) => {
                                    *blocks_since_checkpoint = 0;
                                    *prev_ckpt = (m.height, pos);
                                    *last_written_root = Some(m.root_hash);
                                }
                                Err(e) => tracing::warn!(
                                    target: "ducktape::recovery",
                                    node = %label,
                                    error = %e,
                                    "post-cutover checkpoint write failed; the cutover journal record \
                                     covers a restart"
                                ),
                            },
                            Err(e) => tracing::warn!(
                                target: "ducktape::recovery",
                                node = %label,
                                error = %e,
                                "post-cutover checkpoint capture failed; the cutover journal record \
                                 covers a restart"
                            ),
                        }
                        // THE CUTOVER CHECKPOINT IS NOT GATED — a restart must land on
                        // the new epoch's boundary — but it costs the loop exactly what
                        // the periodic one does, so it is charged the same cooldown.
                        // Otherwise the periodic branch fires immediately after it and
                        // the node pays twice back to back.
                        let post_cutover_cost = context
                            .current()
                            .duration_since(post_cutover_started)
                            .unwrap_or_default();
                        *checkpoint_not_before =
                            cooldown_until(context.current(), post_cutover_cost);
                        tracing::info!(
                            target: "ducktape::consensus",
                            node = %label,
                            epoch = plan.epoch(),
                            validators = members.len(),
                            base_height = plan.cutover_app_height(),
                            "cutover complete: epoch {} with {} validators",
                            plan.epoch(),
                            members.len()
                        );
                    }
                }
            }
        }

        // periodic checkpoint: AFTER the orchestration step, and that order is
        // correctness, not tidiness. The manifest records
        // `pending_cutover_view` off the orchestrator, and a valset change
        // drained in THIS pass arms the cutover in `observe_members` above —
        // so a checkpoint written before it lands on the changing block's own
        // height carrying `None`. On restart `derive_pending_boot` then has
        // nothing to re-arm from either (its fallback scans only blocks ABOVE
        // the manifest height, seeded with the already-changed valset root),
        // no view ceiling is set, and the node applies the very views its
        // peers discarded and cuts over one view late — a silent fork. The
        // observation barrier makes that alignment exact, not unlikely: the
        // drain breaks right after the batch that moved the valset root.
        //
        // Nothing here is owed to the cutover pass: the respawn branch above
        // writes its own unconditional post-cutover manifest and zeroes
        // `blocks_since_checkpoint`, so this branch simply does not fire on
        // that pass.
        //
        // It also snapshots the in-memory cohort and
        // prunes the op journal below the PREVIOUS checkpoint once
        // the persisted floor has passed it (pruned frames must
        // never be needed to resolve a re-reported finalization).
        // A BLOCK COUNT ALONE CANNOT EXPRESS WHAT A CHECKPOINT COSTS. 32 blocks
        // is ~30 s of chain, and one capture measured 59–70 s on a real
        // workspace (#1018) — so the trigger fired again before the previous
        // one had finished paying for itself, and the node spent two thirds of
        // its life in this branch, unable to poll any other arm of the loop.
        // The cadence is therefore gated on BOTH: enough blocks, and enough
        // recovery time since the last attempt to keep the loop's occupancy
        // under one part in `CHECKPOINT_DUTY_LIMIT`.
        // ...and on the state having MOVED. The sealed boundary's root-hash is
        // free here (the drain already stamped it), so an idle chain's nop
        // blocks no longer buy a full re-encode of the same manifest every 32
        // blocks — see `checkpoint_due` and `IDLE_CHECKPOINT_BLOCKS`.
        if let Some(f) = node.finalized()
            && checkpoint_due(
                *blocks_since_checkpoint,
                checkpoint_blocks,
                context.current(),
                *checkpoint_not_before,
                f.root_hash,
                *last_written_root,
            )
        {
            // EVERY STAGE BELOW RUNS ON THE SELECT LOOP, so its duration is
            // time the `http_ingress` arm is not polled and `/v1/query` is
            // unserviced (issue #1018). The stage timings ARE the diagnosis,
            // so they ride the checkpoint event itself rather than needing a
            // profiler on a box where the stall only shows under real state.
            let pos = node.sink_mut().oplog_pos().await;
            let checkpoint_started = context.current();
            // the capture reads the LOOP's clock per module (the host and the
            // recovery crate own none) so a slow capture names its module
            // instead of leaving `capture_ms` to be attributed by guesswork.
            let captured = Manifest::capture_timed(
                node.host(),
                Some(f.height),
                orchestrator.epoch(),
                orchestrator.epoch_base(),
                participant_bytes(orchestrator),
                resident_bytes(orchestrator),
                orchestrator.pending_cutover().map(|c| c.cutover_view()),
                pos,
                *next_seq,
                // the root `f.height` SEALED. the host can be AHEAD of it (a
                // code-swap realization that stalls before applying a block
                // still moved the registry), and a manifest labelled with this
                // height but carrying that root is one recovery fatals on.
                Some(f.root_hash),
                || {
                    context
                        .current()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                },
            )
            // the replay guard rides every checkpoint (see
            // `recovery::Manifest::applied_frames`).
            .map(|(m, cost)| (m.with_replay_window(node.replay_window()), cost));
            let captured_at = context.current();
            match captured {
                Ok((m, capture_cost)) => match node.sink_mut().write_manifest(&m).await {
                    Ok(()) => {
                        let written_at = context.current();
                        *blocks_since_checkpoint = 0;
                        *last_written_root = Some(m.root_hash);
                        let floor_passed = matches!(
                            node.sink_mut().floor_cert(),
                            Ok(Some(fc))
                                if prev_ckpt.0.is_none_or(|h| fc.height >= h)
                        );
                        let needed_from = sync_retention.floor();
                        let cuts_served_sync = prune_cuts_served_sync(prev_ckpt.0, needed_from);
                        let action = decide_prune_action(floor_passed, cuts_served_sync);
                        match action {
                            PruneAction::NotDue => {}
                            PruneAction::HeldForSync => {
                                tracing::debug!(
                                    target: "ducktape::statesync",
                                    node = %label,
                                    reason = "sync_retention_floor",
                                    anchor = prev_ckpt.0.unwrap_or_default(),
                                    needed_from = needed_from.unwrap_or_default(),
                                    "oplog prune held for a served sync"
                                );
                            }
                            PruneAction::Prune => {
                                if let Err(e) = node.sink_mut().prune_oplog(prev_ckpt.1).await {
                                    tracing::warn!(
                                        target: "ducktape::recovery",
                                        node = %label,
                                        error = %e,
                                        "oplog prune failed"
                                    );
                                }
                            }
                        }
                        *prev_ckpt = (m.height, pos);
                        let since = |a: std::time::SystemTime, b: std::time::SystemTime| {
                            b.duration_since(a).unwrap_or_default().as_millis()
                        };
                        let done_at = context.current();
                        tracing::info!(
                            target: "ducktape::recovery",
                            event = "node_checkpoint_written",
                            node = %label,
                            height = m.height.unwrap_or_default(),
                            capture_ms = since(checkpoint_started, captured_at),
                            write_ms = since(captured_at, written_at),
                            prune_ms = since(written_at, done_at),
                            capture_modules = %capture_breakdown(&capture_cost)
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            event = "node_checkpoint_failed",
                            node = %label,
                            stage = "write",
                            error = %e
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "ducktape::recovery",
                        event = "node_checkpoint_failed",
                        node = %label,
                        stage = "capture",
                        error = %e
                    );
                }
            }
            // SET OUTSIDE THE MATCH, ON PURPOSE. A capture that FAILS costs the
            // loop everything a successful one does, and the failure path does
            // not reset `blocks_since_checkpoint` — so without this the retry
            // is immediate and the node re-pays the full cost on every drain
            // tick, forever. The cooldown is what makes the failure survivable.
            let attempt = context
                .current()
                .duration_since(checkpoint_started)
                .unwrap_or_default();
            *checkpoint_not_before = cooldown_until(context.current(), attempt);
        }

        // the state-driven pumps, each its own method below: block
        // cadence/heartbeat, code readiness, capability announce,
        // saga crank, conversation timers, dispatch delivery nudge.
        self.pump_heartbeat().await;
        self.pump_code_readiness().await;
        // the committed lane table, into the watch every declared plane
        // binds off. A no-op when the table did not move, so a lane a swap
        // declares binds on the block that declared it and nothing else
        // wakes a waiting plane.
        crate::lane_table::pump(self.node.host()).await;
        self.pump_saga_crank().await;
        self.pump_conversation_inputs().await;
        self.pump_dispatch_nudge().await;

        let Self {
            node,
            next_seq,
            signer,
            label,
            dev_demo,
            expected,
            applied,
            converged,
            workers,
            ..
        } = self;
        let dev_demo = *dev_demo;
        let expected = *expected;

        // the reactor seam: offer each finalized block's events to
        // the host-owned workers; a claiming worker's follow-up op
        // re-enters through the ordered lane as its own block (the
        // oracle-as-op). events no worker claims are the plain
        // observability stream — only decodable-but-unhandled worker
        // requests would indicate a saga stuck Pending.
        // one drain can apply MANY blocks; the events accumulated across all of
        // them, so stamp them with the drain's finalized tip.
        let height = node.finalized().map_or(0, |f| f.height);
        // the same worker routing the noded submit lane and the sim run — but
        // each claimed follow-up re-enters the ORDERED lane as its own batch
        // (not an inline block), so this lane SUBMITS the follows rather than
        // inline-draining them: the continuous block cadence carries them.
        let host::worker::Offered { follows, unclaimed } =
            host::worker::offer(workers.as_slice(), node.take_events()).await;
        for follow in follows {
            let seq = *next_seq;
            *next_seq += 1;
            if let Err(e) = node.submit(signer, seq, follow).await {
                tracing::warn!(
                    target: "ducktape::modules",
                    node = %label,
                    height,
                    error = %e,
                    "worker follow-up submit failed"
                );
            }
        }
        // an unclaimed event is the module's ONLY diagnostic channel (a wasm
        // guest cannot log) — unless it decodes as a worker request, which
        // means a saga is stuck Pending.
        let mut notes = noded::log::ModuleNotes::new(height);
        for eff in &unclaimed {
            notes.unclaimed(eff);
        }
        notes.finish();
        if dev_demo && !*converged && *applied >= expected {
            // the board is read BEFORE the marker so the marker can carry it.
            // `applied` counts a finalized frame however it was dispositioned
            // (`kernel/node/src/lib.rs:1393-1399`), so a REJECTED seed latches
            // this just as an applied one does — `seeds=<landed>/<expected>` on
            // the one line everything already greps is what makes a seed that
            // never landed visible at all.
            //
            // the signal is ONE-WAY, and reading it as two-way would be wrong:
            // the latch counts BATCHES, heartbeat nops included
            // (`kernel/node/src/lib.rs:1362-1367`), so on a multi-node demo a
            // peer's seed can still be unproposed when this node latches — a
            // healthy `large_file_e2e` run can print `seeds=1/2`. `seeds=N/N`
            // proves the seeds landed; a short count means "not landed AT THIS
            // INSTANT", never "never landed". ONE query: the demo seeds are a
            // handful of tasks, so the board's first `List` page holds them all.
            let reply = node
                .host()
                .query(
                    "tasks",
                    &encode_task_query(&TaskQuery::List {
                        limit: tasks::MAX_LIST_LIMIT,
                        after: None,
                    }),
                )
                .await
                .expect("tasks query");
            let TaskReply::Tasks(board) = decode_task_reply(&reply).expect("task reply") else {
                unreachable!("a list answers a page");
            };
            // the count goes BEFORE the marker, and that is load-bearing: the
            // harness reads a marker as the REST OF ITS LINE
            // (`tests/common/mod.rs:1783-1788`) and `cluster_e2e.rs:154-155`
            // compares that whole remainder against the genesis marker's to
            // prove ops applied. anything appended after the hash — a tracing
            // field included — would make that `assert_ne!` vacuously true.
            let seeds_landed = board.iter().filter(|t| is_a_dev_seed(&t.id)).count();
            let h = node.root_hash();
            tracing::info!(
                target: "ducktape::consensus",
                "node={label} seeds={seeds_landed}/{expected} converged root_hash={}",
                hex(&h)
            );
            // then one line per task so the demo can eyeball the ops (each node
            // ends holding the task it originated AND the peer's). a title is
            // arbitrary user text on a shared board — render it Debug so an
            // embedded newline or quote cannot split the log event.
            for task in board {
                tracing::debug!(
                    target: "ducktape::modules",
                    node = %label,
                    task_id = %task.id,
                    title = ?task.title,
                    "demo task"
                );
            }
            *converged = true;
        }
    }

    // BLOCK CADENCE + heartbeat. `submit`/`submit_frame` ENQUEUE into the
    // node's `pending_batch`; a flush packs every frame the window holds into
    // batch super-frames, each proposed as a single block. flushing happens on
    // TWO paths:
    //
    // - BUSY — [`ValidatorRuntime::pump_eager_flush`], event-driven, NO
    //   interval: the run loop flushes at the end of any turn that left real
    //   ops pending while nothing of ours is in flight. the network's own
    //   agreement speed is the pacer; this heartbeat arm plays no part in
    //   busy pacing.
    // - IDLE — this arm: an empty window beats one nop per block time.
    //   finalized views only advance with a proposed frame, so an idle
    //   network would freeze (its height never ticks and a pending cutover,
    //   which crosses only when finalized views REACH it, would park
    //   forever). the nop targets an unregistered module: it rejects
    //   identically everywhere and leaves no state. a chain with real ops
    //   needs no nop — the ops ARE the blocks.
    //
    // GATE the idle nop on an empty window AND an empty orderer FIFO: a nop
    // pushed while real ops wait (or while a batch still awaits finalization)
    // only piles behind them — a flapping quorum peer would stack idle
    // blocks. a due beat that finds the chain non-quiet RESTAMPS the grid
    // instead, so the nop returns one full block time after the chain goes
    // quiet, never the instant a stall clears.
    async fn pump_heartbeat(&mut self) {
        let now = self.context.current();
        let heartbeat_due =
            now.duration_since(self.last_flush).unwrap_or_default() >= self.cadence.block_time;
        let ops_pending = self.node.pending_batch_len() > 0;
        let orderer_idle = self.node.orderer().pending_len() == 0;
        match heartbeat_action(
            self.heartbeat_disabled,
            ops_pending,
            heartbeat_due,
            orderer_idle,
        ) {
            HeartbeatAction::Idle => {}
            HeartbeatAction::Restamp => self.last_flush = now,
            HeartbeatAction::BeatNop => self.beat_nop(now).await,
        }
    }

    /// the BUSY block path — event-driven, no interval anywhere. the run loop
    /// calls this at the end of EVERY turn: whatever the turn enqueued (an
    /// ingress submit, a relayed frame, a drain-arm system op) flushes into a
    /// proposed block immediately — unless a batch of ours is already in
    /// flight, in which case everything aggregates until that batch clears
    /// and the finalization delivery wake turns the loop again. that in-flight
    /// gate is the whole aggregation story: a lone op ships instantly, and
    /// under load a block naturally carries everything that arrived during
    /// the previous block's consensus round — block size scales with traffic,
    /// with no timer and no 1-tx-1-block regime.
    pub(super) async fn pump_eager_flush(&mut self) {
        let ops_pending = self.node.pending_batch_len() > 0;
        let orderer_idle = self.node.orderer().pending_len() == 0;
        if eager_flush_due(self.heartbeat_disabled, ops_pending, orderer_idle) {
            let now = self.context.current();
            let flushed = self.flush_window(now).await;
            // these batches carry REAL ops (the idle nop flushes via
            // `beat_nop`, never here) — they are what the leader-nudge
            // escort walks through other validators' idle views.
            if flushed > 0 {
                self.real_work_parked = true;
            }
        }
    }

    /// escort parked REAL work through other validators' idle views: while
    /// our un-finalized proposals include real ops and the (locally
    /// estimated) current view's leader is someone else, nudge that leader to
    /// close its view now. one nudge per estimated view — every finalized
    /// block moves the estimate and re-arms the next — so the escort advances
    /// at network speed and goes silent the moment nothing real is parked. a
    /// mis-aimed nudge (stale tip estimate, mid-cutover membership) costs at
    /// most one deterministic nop on the receiver; correctness never depends
    /// on the aim. a parked idle NOP is deliberately not escorted — the 1s
    /// beat owns that pace.
    pub(super) async fn pump_leader_nudge(&mut self) {
        let orderer_idle = self.node.orderer().pending_len() == 0;
        if orderer_idle {
            // nothing of ours awaits finalization — the escort stands down.
            self.real_work_parked = false;
            self.last_nudged_view = None;
            return;
        }
        if !self.real_work_parked {
            return;
        }
        let current_view = self
            .node
            .orderer()
            .newest_finalized_view()
            .map_or(1, |tip| tip + 1);
        if self.last_nudged_view == Some(current_view) {
            return;
        }
        let epoch = self.orchestrator.epoch();
        let members = self.orchestrator.current_members();
        let Some(leader) = round_robin_leader(epoch, current_view, members) else {
            return;
        };
        if *leader == self.signer.public_key() {
            // our own view: propose serves our queue by itself.
            return;
        }
        let leader = leader.clone();
        self.last_nudged_view = Some(current_view);
        crate::relay_runtime::send_nudge(&mut self.relay_tx, &leader);
        tracing::debug!(
            target: "ducktape::consensus",
            node = %self.label,
            view = current_view,
            "leader nudge sent"
        );
    }

    /// a peer validator holds real parked proposals and (by its local
    /// estimate) we lead the current view: close it NOW by beating the idle
    /// nop early, so rotation reaches the parked work at network speed
    /// instead of the 1s idle beat. gated exactly like the beat — quiet chain
    /// only — so a nudge can never pile a nop behind real work or an
    /// in-flight batch, and repeated nudges self-limit (the parked nop keeps
    /// the orderer non-idle until it finalizes). only a CURRENT validator's
    /// transport identity is honored: the nudge is harmless by construction,
    /// but nobody else gets to tick our view clock.
    pub(super) async fn on_leader_nudge(&mut self, peer: &ed25519::PublicKey) {
        let from_current_validator = self.orchestrator.current_members().contains(peer);
        if !from_current_validator {
            return;
        }
        let ops_pending = self.node.pending_batch_len() > 0;
        let orderer_idle = self.node.orderer().pending_len() == 0;
        let beat_now = heartbeat_action(self.heartbeat_disabled, ops_pending, true, orderer_idle)
            == HeartbeatAction::BeatNop;
        if beat_now {
            self.beat_nop(self.context.current()).await;
        }
    }

    /// submit one heartbeat nop, then flush it as the idle beat's block.
    async fn beat_nop(&mut self, now: std::time::SystemTime) {
        let Self {
            node,
            next_seq,
            signer,
            label,
            ..
        } = self;
        let seq = *next_seq;
        *next_seq += 1;
        if let Err(e) = node
            .submit(
                signer,
                seq,
                Msg {
                    target: NOP_TARGET.into(),
                    payload: Vec::new(),
                },
            )
            .await
        {
            tracing::debug!(
                target: "ducktape::submit",
                node = %label,
                error = %e,
                "heartbeat nop submit failed"
            );
        }
        self.flush_window(now).await;
    }

    /// restart the beat grid, then flush the window: no-op when
    /// `pending_batch` is empty (idle with a batch already in flight — wait
    /// for it). returns the number of batches proposed (0 on a no-op or a
    /// failed flush).
    async fn flush_window(&mut self, now: std::time::SystemTime) -> usize {
        self.last_flush = now;
        match self.node.flush_batch().await {
            Ok(batches) => batches,
            Err(e) => {
                tracing::debug!(
                    target: "ducktape::submit",
                    node = %self.label,
                    error = %e,
                    "batch flush failed"
                );
                0
            }
        }
    }

    // CODE READINESS: the byte-receipt half of a pending modreg swap, and
    // the byte PULL lane beside it. EVERY member checks the committed pending
    // swaps and the OPEN code ballots against its LOCAL blob store and spawns
    // one ranged mesh fetch per digest it lacks (the custodian's data-plane
    // push normally lands first — this heals a node the push missed, and
    // carries a proposal's bytes to a node that must hold them before the
    // ballot closes). A CURRENT boundary member additionally earns one
    // truthful validator-origin `SignalReady` per verified-resident swap
    // (the covering signal latches the swap `ready` in consensus).
    // state-driven and idempotent; inert while nothing is pending or open.
    async fn pump_code_readiness(&mut self) {
        let Self {
            node,
            orchestrator,
            next_seq,
            signer,
            label,
            code_signaller,
            index,
            blob_client,
            blobs,
            code_registry,
            proposed_code,
            fetch_done_tx,
            fetch_done_rx,
            ..
        } = self;
        // reap finished fetch tasks first, so a failed fetch retries — on a
        // backoff, speaking only on the first failure and every Nth after it.
        code_signaller.reap_fetches(fetch_done_rx, label);
        code_signaller.tick_fetch_backoff();
        let req = modules::encode_query(&modules::ModulesQuery::ModuleStatus);
        let Ok(bytes) = node.host().query(host::MODULES_ID, &req).await else {
            return; // registry absent: byte-identical drain on a baseline net.
        };
        let Ok(modules::ModulesReply::ModuleStatus { modules }) = modules::decode_reply(&bytes)
        else {
            return;
        };
        // an OPEN RegisterModule/UpdateModule ballot names its bytes too
        // (#1861). the registry cannot name a brand-new artifact until that
        // ballot passes, yet the validators deciding it are exactly the ones
        // that must hold the bytes — naming only the registry deadlocks every
        // `register`. read beside the registry, but only when governance's
        // ROOT has moved: the walk instantiates governance's guest, and doing
        // that on every tick as well as the registry read put this loop past
        // the RPC lane's 10 s deadline. nothing but a governance state change
        // can move the set, and the root is exactly that.
        let proposed =
            crate::validator::code_announce::proposed_code_blobs(node.host(), proposed_code).await;
        // the code plane's push admission gate reads THIS set (#1833): a
        // digest nothing here names any more is refused before any staging.
        // reclaim rides the same registry-change point — whatever fell out
        // (a cancelled/replaced pending swap, or a closed proposal) is
        // forgotten, so an unreferenced blob does not outlive the record that
        // once justified it. activation history stays referenced because
        // checkpoint restore and replay use it.
        let mut referenced = crate::code_plane::code_blobs_referenced(&modules);
        referenced.extend(proposed.iter().copied());
        for digest in code_registry.update(referenced) {
            blobs.forget(&digest);
        }
        // the ONE role read: a seat in the current boundary signals; anyone
        // else running this loop (a seat that rotated out) only pulls.
        let seated = orchestrator
            .current_members()
            .contains(&signer.public_key());
        let role = match seated {
            true => Role::Validator,
            false => Role::Resident,
        };
        // residency is a VERIFYING read (content re-hashed on the disk path)
        // AND a LOADABILITY read: signing ready must mean sha256(local bytes)
        // == committed hash AND "this binary can instantiate them" AND "this
        // host can realize the shape they declare" (an odb substrate for the
        // id, config keys the network binds). Byte residency alone would let
        // a validator on an older build arm a swap it then deterministically
        // rejects every op to while its peers apply them — a silent fork on
        // activation — and a shape the boundary cannot realize would fail
        // closed on every validator at once.
        //
        // WHAT IT COSTS: the probe COMPILES the component synchronously on
        // this select loop — a few hundred ms for a 1.8 MB module, during
        // which `http_ingress` is unpolled, the same occupancy the checkpoint
        // branch carries a duty cooldown for (#1018). It is paid at most once
        // per pending swap per boot — `decide` latches the verdict, loadable
        // and unloadable alike — and every validator pays it at the same
        // moment, right after the swap commits.
        let height = node.finalized().map_or(0, |f| f.height);
        let held = |digest: &[u8; 32]| blobs.has_verified_chunk(digest);
        let verdict = |entry: &modules::ModuleCode, digest: &[u8; 32]| {
            let Some(bytes) = blobs.get_chunk(digest) else {
                return CodeVerdict::Absent;
            };
            let module_id = entry.module_id.as_str();
            // what "this node can run it" means is the entry's kind: a
            // module's bytes must instantiate here AND replace the running
            // module's state shape; a view's bytes must speak the view ABI,
            // and no running module is asked about them.
            let realizable =
                noded::compose::validate_deployment(module_id, entry.kind, &bytes, index)
                    .and_then(|()| match entry.kind {
                        modules::Kind::Module => node
                            .check_module_replacement(module_id, &bytes)
                            .map_err(|error| error.to_string()),
                        modules::Kind::View => Ok(()),
                    });
            match realizable {
                Ok(()) => CodeVerdict::Loadable,
                // the first line only: a wasmtime error carries a multi-line
                // trace, and the whole thing would evict the ring it is
                // evidence in.
                Err(detail) => CodeVerdict::Unloadable {
                    detail: detail.lines().next().unwrap_or_default().to_string(),
                },
            }
        };
        let actions = code_signaller.decide(role, height, &modules, &proposed, held, verdict);
        crate::validator::code_announce::spawn_fetches(
            actions.fetches,
            blob_client,
            blobs,
            fetch_done_tx,
        );
        for (key, detail) in actions.refusals {
            tracing::warn!(
                target: "ducktape::modules",
                node = %label,
                module = %key.module_id,
                swap = key.name,
                reason = "code_not_loadable",
                detail = %detail,
                "pending-swap code refused: this binary cannot instantiate it, so \
                 this node will not signal ready"
            );
        }
        for (key, msg) in actions.signals {
            let seq = *next_seq;
            *next_seq += 1;
            match node.submit(signer, seq, msg).await {
                Ok(_) => tracing::info!(
                    target: "ducktape::modules",
                    node = %label,
                    module = %key.module_id,
                    swap = key.name,
                    "code-ready signaled"
                ),
                Err(e) => {
                    // un-latch so a transient submit failure retries next tick.
                    code_signaller.unlatch(&key);
                    tracing::debug!(
                        target: "ducktape::modules",
                        node = %label,
                        error = %e,
                        "code readiness submit failed; retrying"
                    );
                }
            }
        }
    }

    // SAGA CRANK (P7 liveness, host side): nothing else ever
    // submits `SagaMsg::Crank`, and under strict leases a
    // saga whose assignee went dark advances ONLY via a crank
    // (lease re-lease or deadline timeout). state-driven:
    // when the committed next expiry is at or past the latest
    // finalized height, push one permissionless crank —
    // throttled like the heartbeat, since a backlog wider
    // than CRANK_BUDGET legitimately needs several. duplicate
    // cranks from other nodes are deterministic no-ops.
    async fn pump_saga_crank(&mut self) {
        let Self {
            context,
            node,
            next_seq,
            signer,
            label,
            last_crank,
            cadence,
            ..
        } = self;
        let now = context.current();
        let crank_due = now.duration_since(*last_crank).unwrap_or_default() >= cadence.block_time;
        if crank_due
            && let Some(finalized_height) = node.finalized().map(|f| f.height)
            && let Some(expiry) = saga_next_expiry(node.host()).await
            && expiry <= finalized_height
        {
            *last_crank = now;
            let seq = *next_seq;
            *next_seq += 1;
            if let Err(e) = node
                .submit(
                    signer,
                    seq,
                    Msg {
                        target: "saga".into(),
                        payload: saga::encode_msg(&saga::SagaMsg::Crank {}),
                    },
                )
                .await
            {
                tracing::debug!(
                    target: "ducktape::saga",
                    node = %label,
                    error = %e,
                    "saga crank submit failed"
                );
            } else {
                tracing::debug!(
                    target: "ducktape::saga",
                    node = %label,
                    next_expiry = expiry,
                    finalized_height,
                    "saga crank submitted"
                );
            }
        }
    }

    /// This validator's consensus clock is its committed height. Inspect a
    /// timer only when that clock changes, not when a host wall-clock tick
    /// fires. Duplicate cranks from other validators are deterministic no-ops.
    async fn pump_conversation_inputs(&mut self) {
        let Some(height) = self.node.finalized().map(|block| block.height) else {
            return;
        };
        let already_inspected = self.last_conversation_height == Some(height);
        if already_inspected {
            return;
        }
        self.last_conversation_height = Some(height);
        let due = conversation_next_input_due(self.node.host()).await;
        let Some(msg) = conversation_input_crank(height, due) else {
            return;
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Err(error) = self.node.submit(&self.signer, seq, msg).await {
            tracing::debug!(
                target: "ducktape::agent",
                node = %self.label,
                reason = "conversation_crank_submit_failed",
                error = %error,
                height,
                "conversation timer submit failed; retrying at the next committed block"
            );
        }
    }

    // DISPATCH DELIVERY NUDGE (never-pop-stack liveness): a
    // result committed into the dispatch mailbox delivers via
    // the drain's DeliverPending injection in the NEXT
    // successful block — and heartbeat nops are rejected
    // frames that never apply, so a quiet chain would sit on
    // its mailbox. state-driven: while the committed mailbox
    // is non-empty, push one permissionless Nudge — a no-op
    // whose block carries the injection. duplicate nudges
    // from other nodes are free.
    async fn pump_dispatch_nudge(&mut self) {
        let Self {
            context,
            node,
            next_seq,
            signer,
            label,
            last_nudge,
            cadence,
            ..
        } = self;
        let now = context.current();
        let nudge_due = now.duration_since(*last_nudge).unwrap_or_default() >= cadence.block_time;
        if nudge_due && dispatch_pending_deliveries(node.host()).await > 0 {
            *last_nudge = now;
            let seq = *next_seq;
            *next_seq += 1;
            if let Err(e) = node
                .submit(
                    signer,
                    seq,
                    Msg {
                        target: "dispatch".into(),
                        payload: dispatch::encode_msg(&dispatch::DispatchMsg::Nudge {}),
                    },
                )
                .await
            {
                tracing::debug!(
                    target: "ducktape::saga",
                    node = %label,
                    error = %e,
                    "dispatch delivery nudge submit failed"
                );
            } else {
                tracing::debug!(
                    target: "ducktape::saga",
                    node = %label,
                    "dispatch delivery nudge submitted"
                );
            }
        }
    }
}

/// what one heartbeat beat does — the pure decision behind
/// [`ValidatorRuntime::pump_heartbeat`], separate so the idle cadence stays
/// unit-testable without a runtime. the BUSY path is [`eager_flush_due`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatAction {
    /// the beat is not due (or the heartbeat is disabled).
    Idle,
    /// the beat is due but the chain is not quiet — ops are pending (the
    /// eager path owns them) or a batch is still in flight. restart the beat
    /// grid without flushing, so the nop returns one full block time after
    /// the chain goes quiet.
    Restamp,
    /// the beat is due on a quiet chain: submit one nop, then flush it.
    BeatNop,
}

fn heartbeat_action(
    disabled: bool,
    ops_pending: bool,
    heartbeat_due: bool,
    orderer_idle: bool,
) -> HeartbeatAction {
    if disabled {
        return HeartbeatAction::Idle;
    }
    if !heartbeat_due {
        return HeartbeatAction::Idle;
    }
    let chain_quiet = !ops_pending && orderer_idle;
    if chain_quiet {
        HeartbeatAction::BeatNop
    } else {
        HeartbeatAction::Restamp
    }
}

/// the pure decision behind [`ValidatorRuntime::pump_eager_flush`]: flush the
/// moment real ops are pending with nothing of ours in flight — event-driven,
/// no interval. the in-flight gate (`orderer_idle`) is what makes blocks
/// aggregate under load instead of reviving the 1-tx-1-block regime.
fn eager_flush_due(disabled: bool, ops_pending: bool, orderer_idle: bool) -> bool {
    !disabled && ops_pending && orderer_idle
}

/// is this task id one the dev-demo boot seed minted? the seed writes exactly
/// `k{node-label}` (`validator/engine.rs:273-283`), and nothing else that
/// reaches a demo board is shaped like that. counting the WHOLE board instead
/// would let any unrelated write mask a seed that never landed — restart_e2e's
/// own writes land before the converge latch and do exactly that.
fn is_a_dev_seed(task_id: &str) -> bool {
    task_id
        .strip_prefix('k')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// the round-robin leader for `view`, MIRRORING the engine's elector
/// (`RoundRobin::<Sha256>::default()` — UNSHUFFLED, so the permutation is the
/// identity over the sorted participant set and the index is
/// `(epoch + view) % n`). a drifted mirror only mis-aims a nudge — the nudged
/// peer beats at most one harmless nop — so consensus never depends on this
/// staying in sync with the engine.
fn round_robin_leader(
    epoch: u64,
    view: u64,
    participants: &std::collections::BTreeSet<ed25519::PublicKey>,
) -> Option<&ed25519::PublicKey> {
    if participants.is_empty() {
        return None;
    }
    let index = epoch.wrapping_add(view) as usize % participants.len();
    participants.iter().nth(index)
}

/// the committed dispatch mailbox's undelivered-result count — the nudge
/// pump's read. `0` when the module is absent or the mailbox is empty.
pub(crate) async fn dispatch_pending_deliveries(host: &host::Host) -> u64 {
    use dispatch::{DispatchQuery, DispatchReply, decode_reply, encode_query};
    let Ok(reply) = host
        .query("dispatch", &encode_query(&DispatchQuery::PendingDeliveries))
        .await
    else {
        return 0;
    };
    match decode_reply(&reply) {
        Ok(DispatchReply::PendingDeliveries(n)) => n,
        _ => 0,
    }
}

/// the committed saga ledger's earliest pending lease-expiry/deadline — the
/// crank pump's read. `None` when the module is absent or nothing pending
/// carries one.
pub(crate) async fn saga_next_expiry(host: &host::Host) -> Option<u64> {
    use saga::{SagaQuery, SagaReply, decode_reply, encode_query};
    let reply = host
        .query("saga", &encode_query(&SagaQuery::NextExpiry))
        .await
        .ok()?;
    match decode_reply(&reply).ok()? {
        SagaReply::NextExpiry(v) => v,
        _ => None,
    }
}

/// Earliest durable timer; absence of the Runs module requires no host work.
async fn conversation_next_input_due(host: &host::Host) -> Option<u64> {
    let reply = host
        .query("runs", &runs::encode_query(&runs::RunsQuery::NextConversationInputDue))
        .await
        .ok()?;
    let runs::RunsReply::NextConversationInputDue(due) = runs::decode_reply(&reply).ok()? else {
        return None;
    };
    due
}

fn conversation_input_crank(height: u64, due: Option<u64>) -> Option<Msg> {
    let due = due?;
    let timer_ready = due <= height;
    if !timer_ready {
        return None;
    }
    Some(Msg {
        target: "runs".into(),
        payload: runs::encode_msg(&runs::RunsMsg::CrankConversationInputs),
    })
}

#[cfg(test)]
mod conversation_crank_tests {
    use super::conversation_input_crank;

    #[test]
    fn no_timer_or_a_future_consensus_deadline_does_not_submit() {
        assert!(conversation_input_crank(100, None).is_none());
        assert!(conversation_input_crank(100, Some(101)).is_none());
    }

    #[test]
    fn due_and_overdue_inputs_submit_the_permissionless_crank() {
        for due in [99, 100] {
            let msg = conversation_input_crank(100, Some(due)).unwrap();
            assert_eq!(msg.target, "runs");
            assert!(matches!(
                runs::decode_msg(&msg.payload).unwrap(),
                runs::RunsMsg::CrankConversationInputs
            ));
        }
    }
}

#[cfg(test)]
mod prune_retention_tests {
    use super::{PruneAction, decide_prune_action, prune_cuts_served_sync};

    /// a floor that has not passed overrides everything: there is nothing to
    /// prune yet, served sync or not.
    #[test]
    fn floor_not_passed_never_prunes() {
        assert_eq!(decide_prune_action(false, true), PruneAction::NotDue);
        assert_eq!(decide_prune_action(false, false), PruneAction::NotDue);
    }

    /// no syncer, no hold: the checkpoint prunes the moment the floor passes.
    #[test]
    fn nothing_in_flight_prunes_once_the_floor_passes() {
        assert!(!prune_cuts_served_sync(Some(3598), None));
        assert_eq!(decide_prune_action(true, false), PruneAction::Prune);
    }

    /// the reported shape: the syncer is still installing boundary 3404 while
    /// the busy chain's next checkpoint anchor is already 3598. pruning there
    /// answers its catch-up with `RangePruned` and restarts its whole
    /// bootstrap, so the checkpoint holds instead.
    #[test]
    fn a_served_boundary_below_the_anchor_holds_the_prune() {
        assert!(prune_cuts_served_sync(Some(3598), Some(3404)));
        assert_eq!(decide_prune_action(true, true), PruneAction::HeldForSync);
    }

    /// and the hold releases on the syncer's OWN progress — a newer boundary
    /// or a later frame batch — never on a checkpoint count.
    #[test]
    fn the_hold_releases_when_the_syncer_passes_the_anchor() {
        assert!(!prune_cuts_served_sync(Some(3598), Some(3598)));
        assert!(!prune_cuts_served_sync(Some(3598), Some(3660)));
    }

    /// a boot with no previous checkpoint height has no anchor to hold
    /// against, and prunes nothing either way.
    #[test]
    fn no_anchor_holds_nothing() {
        assert!(!prune_cuts_served_sync(None, Some(3404)));
    }
}

#[cfg(test)]
mod block_cadence_tests {
    use super::{HeartbeatAction, eager_flush_due, heartbeat_action};

    #[test]
    fn pending_ops_flush_immediately_with_nothing_in_flight() {
        // the busy path is event-driven: no beat, no tick, no interval —
        // pending ops with an idle orderer flush now.
        assert!(eager_flush_due(false, true, true));
    }

    #[test]
    fn in_flight_batch_aggregates_instead_of_flushing() {
        // one batch in flight at a time: ops arriving during a consensus
        // round pile into the NEXT block — natural aggregation, never
        // 1-tx-1-block.
        assert!(!eager_flush_due(false, true, false));
    }

    #[test]
    fn empty_window_never_eager_flushes() {
        assert!(!eager_flush_due(false, false, true));
        assert!(!eager_flush_due(false, false, false));
    }

    #[test]
    fn idle_beat_waits_for_block_time() {
        assert_eq!(
            heartbeat_action(false, false, false, true),
            HeartbeatAction::Idle
        );
        assert_eq!(
            heartbeat_action(false, false, true, true),
            HeartbeatAction::BeatNop
        );
    }

    #[test]
    fn due_beat_on_a_non_quiet_chain_restamps_without_a_nop() {
        // ops pending (the eager path owns them) or a batch in flight: no
        // nop — restart the grid so the nop returns one full block time
        // after the chain goes quiet.
        assert_eq!(
            heartbeat_action(false, true, true, true),
            HeartbeatAction::Restamp
        );
        assert_eq!(
            heartbeat_action(false, false, true, false),
            HeartbeatAction::Restamp
        );
        assert_eq!(
            heartbeat_action(false, true, true, false),
            HeartbeatAction::Restamp
        );
    }

    #[test]
    fn round_robin_leader_mirrors_the_unshuffled_elector() {
        use commonware_cryptography::{Signer as _, ed25519};
        // three sorted participants: the leader index is (epoch + view) % 3
        // over the SORTED set — the identity permutation the engine's
        // unshuffled RoundRobin elector uses.
        let keys: std::collections::BTreeSet<ed25519::PublicKey> = (0..3u64)
            .map(|seed| ed25519::PrivateKey::from_seed(seed).public_key())
            .collect();
        let sorted: Vec<&ed25519::PublicKey> = keys.iter().collect();
        for view in 0..7u64 {
            let expected = sorted[(5u64.wrapping_add(view)) as usize % 3];
            assert_eq!(
                super::round_robin_leader(5, view, &keys),
                Some(expected),
                "view {view}"
            );
        }
        // rotation advances by exactly one participant per view.
        assert_ne!(
            super::round_robin_leader(5, 1, &keys),
            super::round_robin_leader(5, 2, &keys)
        );
    }

    /// THE BLOCK COUNT ALONE MUST NO LONGER AUTHORIZE A CHECKPOINT. This is the
    /// assertion that fails if the cooldown is dropped from the decision —
    /// testing the arithmetic of `cooldown_until` on its own would not, since
    /// nothing would be consulting it.
    #[test]
    fn a_sealed_block_count_alone_does_not_authorize_an_expensive_checkpoint() {
        let finished = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let owed =
            crate::drain_actions::cooldown_until(finished, std::time::Duration::from_secs(60));
        // real work sealed since the last manifest, so the change gate is open
        // throughout and the cooldown is the ONLY thing under test here.
        let moved = sdk::StateRoot([9; sdk::ROOT_LEN]);

        // 32 blocks have sealed — the ENTIRE old trigger — but only ~30s of
        // chain has passed, which is exactly the shape that had the node
        // spending two thirds of its life inside the branch.
        assert!(
            !crate::drain_actions::checkpoint_due(
                32,
                32,
                finished + std::time::Duration::from_secs(30),
                owed,
                moved,
                None
            ),
            "a 60s checkpoint must not be re-authorized 30s later just because 32 blocks sealed"
        );
        // ...and it does fire once the cost has been paid back.
        assert!(crate::drain_actions::checkpoint_due(
            32,
            32,
            finished + std::time::Duration::from_secs(420),
            owed,
            moved,
            None
        ));
        // A CHEAP CHECKPOINT IS NEVER DELAYED: 25ms owes 175ms of quiet, long
        // gone by the time 32 blocks seal, so the configured cadence governs.
        let cheap =
            crate::drain_actions::cooldown_until(finished, std::time::Duration::from_millis(25));
        assert!(crate::drain_actions::checkpoint_due(
            32,
            32,
            finished + std::time::Duration::from_secs(30),
            cheap,
            moved,
            None
        ));
        // The cooldown never SUBSTITUTES for the cadence: quiet is necessary,
        // not sufficient.
        assert!(!crate::drain_actions::checkpoint_due(
            31,
            32,
            finished + std::time::Duration::from_secs(420),
            owed,
            moved,
            None
        ));
    }

    /// THE COOLDOWN IS PROPORTIONAL TO THE COST, which is the whole point: a
    /// cheap checkpoint keeps the configured block cadence, an expensive one
    /// backs itself off without anyone tuning a constant per workspace.
    #[test]
    fn a_checkpoint_holds_the_next_one_off_by_what_it_cost() {
        let finished = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        // the demo workspace's pre-#1023 checkpoint: 60s of loop occupancy
        // every 32 blocks (~30s of chain), i.e. it never stopped.
        let expensive =
            crate::drain_actions::cooldown_until(finished, std::time::Duration::from_secs(60));
        assert_eq!(
            expensive,
            finished + std::time::Duration::from_secs(420),
            "a 60s checkpoint must buy 7 minutes of quiet — 1/8 duty"
        );
        // post-#1023: 25ms. The hold is 175ms, far under the 32-block cadence,
        // so the configured cadence still governs and this guard is invisible.
        let cheap =
            crate::drain_actions::cooldown_until(finished, std::time::Duration::from_millis(25));
        assert_eq!(cheap, finished + std::time::Duration::from_millis(175));
    }

    /// A NODE THAT STOPS CHECKPOINTING IS WORSE OFF THAN ONE THAT STUTTERS —
    /// it cannot recover quickly or admit a joiner. So the overflow direction
    /// is "no cooldown", never "never again".
    #[test]
    fn an_absurd_cost_does_not_disable_checkpointing_forever() {
        let finished = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        assert_eq!(
            crate::drain_actions::cooldown_until(finished, std::time::Duration::MAX),
            finished,
            "an unrepresentable cooldown must collapse to none, not to forever"
        );
    }

    #[test]
    fn no_participants_elects_no_leader() {
        let empty = std::collections::BTreeSet::new();
        assert_eq!(super::round_robin_leader(1, 1, &empty), None);
    }

    #[test]
    fn disabled_heartbeat_beats_and_flushes_nothing() {
        for ops_pending in [false, true] {
            for heartbeat_due in [false, true] {
                for orderer_idle in [false, true] {
                    assert_eq!(
                        heartbeat_action(true, ops_pending, heartbeat_due, orderer_idle),
                        HeartbeatAction::Idle
                    );
                    assert!(!eager_flush_due(true, ops_pending, orderer_idle));
                }
            }
        }
    }
}

/// The one ordering in `drain_pass` that is correctness rather than taste,
/// guarded at the source.
#[cfg(test)]
mod drain_order_lint {
    /// A CHECKPOINT MUST NOT BE CAPTURED BEFORE THE PASS HAS OBSERVED THE
    /// MEMBERSHIP IT IS RECORDING. `Manifest::capture_timed` writes
    /// `pending_cutover_view` off the orchestrator, and a valset change drained
    /// in this pass arms that cutover in `observe_members`. Captured first, a
    /// checkpoint landing on the changing block's own height records `None`,
    /// and the boot-time re-arm scan cannot see the change either (it reads
    /// only blocks above the manifest height, seeded with the already-changed
    /// valset root) — so the restart arms no ceiling, applies the views its
    /// peers discarded, and cuts over one view late into a silent fork.
    #[test]
    fn the_periodic_checkpoint_is_captured_after_the_membership_observation() {
        let src = include_str!("drain.rs");
        let observed = src
            .find("actions.observe_members()")
            .expect("the drain observes membership");
        let captured = src
            .find("Manifest::capture_timed")
            .expect("the drain captures a periodic checkpoint");
        assert!(
            observed < captured,
            "the periodic checkpoint must be captured AFTER the valset \
             orchestration step, or a checkpoint on a valset-changing block \
             loses the cutover it armed"
        );
    }
}
