//! the block-projection seam: RootOp assembly, explorer-row bytes, and the
//! member-then-System ordering shared by every block-apply lane.
//!
//! today's callers are the validator drain and the replica park loop; later
//! campaign tasks route the noded submit lane and simnode through the same
//! seam. This is the ONE place a block's decoded contents become an explorer
//! row, so a row reads byte-identically regardless of which lane wrote it —
//! pinned by the golden test below. The seam owns assembly + row bytes only;
//! each lane keeps its own index feed, stream publish, metrics, and receipt
//! shaping (those diverge by role — validator receipts, replica seal checks,
//! per-lane stream cadence — and are not part of the projection).

use sdk::{Origin, StateRoot};

use crate::blobs::BlobHandle;
use crate::{
    BlockDisposition, BlockRecord, DispatchInfo, RootOp, block_row, hex_bytes, hex_root,
    index_block_ops, payload_preview,
};

/// the idle-chain heartbeat filler's target, owned by the kernel (its drain
/// counts the nop apart from real rejections); the projection hides a block
/// whose only op is it, and the heartbeat submits with it, all through this
/// one constant.
pub use node::NOP_TARGET;

fn log_provider_result_tail(height: u64, dispatches: &[host::DispatchRecord]) -> bool {
    let mut observed = false;
    for dispatch in dispatches {
        match dispatch.module.as_str() {
            "saga" => {
                let Ok(saga::SagaMsg::OracleResult {
                    saga_id, attempt, ..
                }) = saga::decode_msg(&dispatch.payload)
                else {
                    continue;
                };
                observed = true;
                tracing::debug!(
                    target: "ducktape::provider",
                    event = "provider_result_tail",
                    phase = "result_canonical",
                    saga = %saga_id,
                    attempt,
                    height,
                    "provider result reached canonical module state"
                );
            }
            "runs" => {
                let Ok(dispatch::Delivery::Result(result)) =
                    dispatch::decode_delivery(&dispatch.payload)
                else {
                    continue;
                };
                observed = true;
                tracing::debug!(
                    target: "ducktape::provider",
                    event = "provider_result_tail",
                    phase = "runs_published",
                    dispatch = %result.dispatch_id,
                    height,
                    "Runs published the provider result"
                );
            }
            "attribution" => match attribution::decode_msg(&dispatch.payload) {
                Ok(attribution::AttributionMsg::Attribute { object, .. })
                    if object.kind == "message" =>
                {
                    let Some(dispatch_id) = object.object.strip_prefix("agent/") else {
                        continue;
                    };
                    let dispatch_id = dispatch_id.split('/').next().unwrap_or(dispatch_id);
                    observed = true;
                    tracing::debug!(
                        target: "ducktape::provider",
                        event = "provider_result_tail",
                        phase = "attribution_published",
                        dispatch = %dispatch_id,
                        message = %object.object,
                        height,
                        "attribution published the message relation"
                    );
                }
                Ok(attribution::AttributionMsg::AttributeBatch { updates }) => {
                    for update in updates {
                        if update.object.kind != "message" {
                            continue;
                        }
                        let Some(dispatch_id) = update.object.object.strip_prefix("agent/") else {
                            continue;
                        };
                        let dispatch_id = dispatch_id.split('/').next().unwrap_or(dispatch_id);
                        observed = true;
                        tracing::debug!(
                            target: "ducktape::provider",
                            event = "provider_result_tail",
                            phase = "attribution_published",
                            dispatch = %dispatch_id,
                            message = %update.object.object,
                            height,
                            "attribution published the message relation"
                        );
                    }
                }
                Ok(attribution::AttributionMsg::Attribute { .. })
                | Ok(attribution::AttributionMsg::Subscribe {})
                | Err(_) => {}
            },
            "chat" => {
                let Ok(chat::ChatMsg::PostMessage {
                    channel_id,
                    message_id,
                    ..
                }) = chat::decode_msg(&dispatch.payload)
                else {
                    continue;
                };
                let Some(dispatch_id) = message_id.strip_prefix("agent/") else {
                    continue;
                };
                let dispatch_id = dispatch_id.split('/').next().unwrap_or(dispatch_id);
                observed = true;
                tracing::debug!(
                    target: "ducktape::provider",
                    event = "provider_result_tail",
                    phase = "chat_message_published",
                    dispatch = %dispatch_id,
                    channel = %channel_id,
                    message = %message_id,
                    height,
                    "Chat committed the provider message"
                );
            }
            _ => {}
        }
    }
    observed
}

/// build one explorer row op ([`RootOp`]) from a block member's decoded parts —
/// THE RootOp assembly seam, shared by the live drain, the boot fold, and (as
/// later tasks adopt it) the noded submit lane and simnode, so every writer
/// produces byte-identical ops. staging the payload IS computing `op_hash`
/// (put_chunk keys the blob by sha256), and on the fold path the re-staging is
/// load-bearing: the blob store is in-memory, so the live drain's staging dies
/// with the process and this is what makes `GET /v1/files/blob/{op_hash}`
/// answer again after a restart.
///
/// a dispatch's origin renders through [`DispatchInfo`] — `Origin::External`
/// flattens to plain `"external"` (the block-level `proposer` already carries
/// the key). that is the ONE shape; the noded submit lane's old `external:<name>`
/// variant is retired as it adopts this seam.
///
/// takes `&dyn Blobs` (not the concrete handle) so the boot-fold path, which
/// holds only a trait object, feeds the same seam.
pub fn project_root_op(
    blobs: &dyn blobstore::Blobs,
    origin: &Origin,
    target: &str,
    payload: &[u8],
    dispatches: &[host::DispatchRecord],
    disposition: BlockDisposition,
) -> RootOp {
    RootOp {
        proposer: match origin {
            Origin::External(key) => hex_bytes(key),
            // frames only carry verified External authorship; label the
            // impossible rest.
            Origin::Module(id) => format!("module:{id}"),
            Origin::Program(account) => format!("acct:{account}"),
            Origin::System => "system".into(),
        },
        disposition,
        target: target.to_string(),
        operations: dispatches.iter().map(DispatchInfo::from).collect(),
        payload: payload_preview(payload),
        op_hash: hex_bytes(&blobs.put_chunk(payload.to_vec())),
    }
}

/// one finalized block's projection: the per-block facts both role loops fold
/// into metrics, receipts, and seal checks, plus the explorer row bytes to feed
/// the derived index (`record` — [`None`] for an idle/nop or discarded-only
/// block the explorer hides).
pub struct BlockProjection {
    pub height: u64,
    pub dispatches: Vec<host::DispatchRecord>,
    pub record: Option<Vec<u8>>,
    pub sealed_hash: Option<StateRoot>,
    pub applied: bool,
    pub latency_us: u64,
    pub applied_ops: usize,
    pub rejected_ops: usize,
}

impl BlockProjection {
    /// DID A BLOCK HAPPEN AT THIS HEIGHT? `sealed_hash` is the discriminant:
    /// every non-discarded frame stamps it, so a rejected-only height (the
    /// idle nop) IS sealed and folds, while a batch every frame of which the
    /// cutover ceiling discarded sealed nothing.
    ///
    /// A DISCARDED HEIGHT MUST NEVER FOLD. Discarded views are `view >=
    /// cutover_view`, at height `old_base + view`, and the new epoch's base IS
    /// `old_base + cutover_view` — so a discarded height collides EXACTLY with
    /// one of the new epoch's first heights. Folding it writes that height into
    /// every module's watermark, and the real block that later lands there is
    /// then skipped by the fold's own `>= block.height` guard: its ops live in
    /// canonical state and in `GET /v1/blocks` but are permanently absent from
    /// every per-module read model, with nothing logged and no self-heal.
    pub fn sealed(&self) -> bool {
        self.sealed_hash.is_some()
    }
}

/// Group a drain's per-frame outcomes into the per-block projections consumed
/// by both role loops. Member dispatches precede System dispatches, matching
/// live indexing and replay; discarded frames retain an empty projection.
pub fn project_block(
    drained: &[node::DrainedFrame],
    system_dispatches: Vec<(u64, Vec<host::DispatchRecord>)>,
    blobs: &BlobHandle,
) -> Vec<BlockProjection> {
    let mut system_dispatches: std::collections::BTreeMap<_, _> =
        system_dispatches.into_iter().collect();
    let mut projections = Vec::new();
    let mut i = 0;
    while i < drained.len() {
        let height = drained[i].height;
        let mut dispatches = Vec::new();
        let mut latency_us = 0u64;
        let mut applied = false;
        let mut ops = Vec::new();
        let mut applied_ops = 0usize;
        let mut rejected_ops = 0usize;
        let mut block_hash = None;
        let mut block_root_hash = None;
        let mut sealed_hash = None;
        while i < drained.len() && drained[i].height == height {
            let frame = &drained[i];
            i += 1;
            if frame.disposition == node::Disposition::Discarded {
                continue;
            }
            sealed_hash = Some(frame.root_hash);
            if let (node::Disposition::Applied, Some(op)) = (&frame.disposition, &frame.op) {
                applied = true;
                latency_us = latency_us.saturating_add(op.latency_us);
                dispatches.extend(op.dispatches.iter().cloned());
            }
            if let Some(op) = &frame.op
                && op.target != NOP_TARGET
            {
                let disposition = match frame.disposition {
                    node::Disposition::Applied => {
                        applied_ops += 1;
                        BlockDisposition::Applied
                    }
                    node::Disposition::Rejected => {
                        rejected_ops += 1;
                        BlockDisposition::Rejected
                    }
                    node::Disposition::Discarded => continue,
                };
                if block_hash.is_none() {
                    block_hash = Some(frame.id);
                    block_root_hash = Some(frame.root_hash);
                }
                ops.push(project_root_op(
                    blobs,
                    &op.origin,
                    &op.target,
                    &op.payload,
                    &op.dispatches,
                    disposition,
                ));
            }
        }
        if let Some(system) = system_dispatches.remove(&height) {
            dispatches.extend(system);
        }
        let record = (!ops.is_empty()).then(|| {
            block_row(&BlockRecord {
                height,
                hash: block_hash.map(|hash| hex_bytes(&hash)).unwrap_or_default(),
                commit_hash: block_root_hash
                    .map(|hash| hex_root(&hash))
                    .unwrap_or_default(),
                ops,
            })
        });
        projections.push(BlockProjection {
            height,
            dispatches,
            record,
            sealed_hash,
            applied,
            latency_us,
            applied_ops,
            rejected_ops,
        });
    }
    projections
}

/// Fold one finalized block into the derived per-module index — the shared
/// epilogue of every block-apply lane (validator drain, embedded daemon submit,
/// sim). First covers every module the host runs at this block (`roots`,
/// the host's module set — a module admitted at this block's boundary gets
/// its database before its first op folds), then merges the lane's explorer
/// `record` onto [`index_block_ops`] and applies it, logging the ONE
/// STALE-index error they share on failure.
///
/// The derived index is a READ MODEL: a failure here degrades the app's views
/// (every module view the app reads is served from it, and it does not
/// self-heal), NEVER consensus — canonical state is already sealed, so the block
/// stands regardless. Hence loud-but-non-fatal. `event` is the operational
/// contract (#603); `target` is the filtering plane — orthogonal, carry both.
pub fn apply_block_to_index(
    index: &indexer::IndexStore,
    height: u64,
    consensus_time: u64,
    record: Option<Vec<u8>>,
    dispatches: &[host::DispatchRecord],
    host: &host::Host,
) {
    let provider_tail_observed = log_provider_result_tail(height, dispatches);
    // a poisoned store refuses every write until an operator rebuilds it, and
    // the ONE error below named that remedy when it happened. re-logging it
    // per block is a log bomb (8k lines in an hour on a real network) that
    // evicts the very line explaining what poisoned the store.
    if index.is_poisoned() {
        tracing::debug!(
            target: "ducktape::consensus",
            height,
            "index apply skipped: store poisoned"
        );
        return;
    }
    if let Err(err) = crate::converge_host_modules(index, host) {
        tracing::error!(
            target: "ducktape::consensus",
            event = "node_index_poisoned",
            height,
            error = %err,
            "module index cannot cover an admitted module — the app's views are now \
             STALE; wipe <storage>/index to rebuild"
        );
        return;
    }
    let ops = indexer::BlockOps {
        record,
        ..index_block_ops(height, consensus_time, dispatches)
    };
    match index.apply_block(&ops) {
        Ok(()) if provider_tail_observed => tracing::debug!(
            target: "ducktape::provider",
            event = "provider_result_tail",
            phase = "index_published",
            height,
            "derived index accepted the committed block"
        ),
        Ok(()) => {}
        Err(err) => {
            tracing::error!(
                target: "ducktape::consensus",
                event = "node_index_poisoned",
                height,
                error = %err,
                "module index apply failed — the app's views are now STALE; wipe \
                 <storage>/index to rebuild"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(module: &str, payload: &[u8]) -> host::DispatchRecord {
        host::DispatchRecord {
            module: module.into(),
            origin: Origin::Module("source".into()),
            cause: sdk::Cause::Direct,
            payload: payload.to_vec(),
            emitted_msgs: 0,
            emitted_events: 0,
            output: None,
            assigned: Vec::new(),
        }
    }

    fn drained(
        id: u8,
        height: u64,
        disposition: node::Disposition,
        root: u8,
        op: Option<node::DrainedOp>,
    ) -> node::DrainedFrame {
        node::DrainedFrame {
            id: [id; 32],
            height,
            disposition,
            root_hash: StateRoot([root; sdk::ROOT_LEN]),
            op,
            reason: None,
        }
    }

    /// GOLDEN: the exact `block_row` bytes this seam produces, captured from the
    /// pre-extraction validator path (`bin/node`'s `block_actions`). Any change
    /// to the projection that shifts these bytes is a wire change to
    /// `GET /v1/blocks` and must be treated as one. Covers: applied member,
    /// rejected member, System dispatch fold, multi-member block, and the two
    /// empty-batch-no-row shapes (nop-only + System dispatch, and
    /// discarded-only).
    #[test]
    fn project_block_row_bytes_are_golden() {
        let blobs = BlobHandle::default();
        let member = dispatch("chat", b"member");
        let system = dispatch("modules", b"system");
        let frames = vec![
            // multi-member block: applied + rejected members, plus a System
            // dispatch folded into the block's dispatch stream (never a row).
            drained(
                1,
                7,
                node::Disposition::Applied,
                9,
                Some(node::DrainedOp {
                    origin: Origin::External(vec![1, 2, 3]),
                    target: "chat".into(),
                    payload: b"hello world".to_vec(),
                    dispatches: vec![member.clone()],
                    latency_us: 11,
                }),
            ),
            drained(
                2,
                7,
                node::Disposition::Rejected,
                9,
                Some(node::DrainedOp {
                    origin: Origin::External(vec![4, 5, 6]),
                    target: "chat".into(),
                    payload: b"nope".to_vec(),
                    dispatches: Vec::new(),
                    latency_us: 99,
                }),
            ),
            // a second applied member, at its own height.
            drained(
                4,
                8,
                node::Disposition::Applied,
                10,
                Some(node::DrainedOp {
                    origin: Origin::External(vec![7]),
                    target: "tasks".into(),
                    payload: b"parent".to_vec(),
                    dispatches: vec![member.clone()],
                    latency_us: 3,
                }),
            ),
            // empty-batch-no-row: a nop-only height that still folds a System
            // dispatch — no explorer row, but the dispatch advances the index.
            drained(
                5,
                9,
                node::Disposition::Rejected,
                11,
                Some(node::DrainedOp {
                    origin: Origin::External(vec![8]),
                    target: NOP_TARGET.into(),
                    payload: Vec::new(),
                    dispatches: Vec::new(),
                    latency_us: 0,
                }),
            ),
            // discarded-only height: no row either.
            drained(6, 10, node::Disposition::Discarded, 12, None),
        ];

        let projections = project_block(
            &frames,
            vec![(7, vec![system.clone()]), (9, vec![system.clone()])],
            &blobs,
        );

        let rows: Vec<(u64, Option<String>)> = projections
            .iter()
            .map(|p| {
                (
                    p.height,
                    p.record
                        .as_deref()
                        .map(|b| std::str::from_utf8(b).unwrap().to_string()),
                )
            })
            .collect();

        assert_eq!(
            rows,
            vec![
                (7, Some(r#"{"height":7,"hash":"0101010101010101010101010101010101010101010101010101010101010101","commit_hash":"0909090909090909090909090909090909090909090909090909090909090909","ops":[{"proposer":"010203","disposition":"applied","target":"chat","operations":[{"module":"chat","origin":"module:source","emitted_msgs":0,"emitted_events":0}],"payload":"hello world","op_hash":"b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"},{"proposer":"040506","disposition":"rejected","target":"chat","operations":[],"payload":"nope","op_hash":"ca3704aa0b06f5954c79ee837faa152d84d6b2d42838f0637a15eda8337dbdce"}]}"#.to_string())),
                (8, Some(r#"{"height":8,"hash":"0404040404040404040404040404040404040404040404040404040404040404","commit_hash":"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a","ops":[{"proposer":"07","disposition":"applied","target":"tasks","operations":[{"module":"chat","origin":"module:source","emitted_msgs":0,"emitted_events":0}],"payload":"parent","op_hash":"e47125968b3b71049fbc4802d1e40a71ea1359decfabacf70b34588037d4ff0c"}]}"#.to_string())),
                (9, None),
                (10, None),
            ]
        );

        // the non-row facts the role loops fold: the System dispatch rides the
        // block's dispatch stream after the members; the nop-only height (9)
        // still carries its System dispatch; discarded (10) carries nothing.
        assert_eq!(
            projections[0].dispatches,
            vec![member.clone(), system.clone()]
        );
        assert_eq!(
            (projections[0].applied_ops, projections[0].rejected_ops),
            (1, 1)
        );
        assert!(projections[0].applied);
        assert_eq!(projections[0].latency_us, 11);
        assert_eq!(projections[1].dispatches, vec![member.clone()]);
        assert_eq!(projections[2].dispatches, vec![system]);
        assert!(!projections[2].applied);
        assert!(projections[3].dispatches.is_empty());
        assert_eq!(projections[3].sealed_hash, None);
    }

    /// A CUTOVER-DISCARDED BATCH IS NOT A BLOCK. Its height is, by
    /// construction, one the NEW epoch is about to use (`old_base + view` for
    /// `view >= cutover_view`, and the new base IS `old_base + cutover_view`),
    /// so folding it advances every module's watermark past the new epoch's
    /// first real blocks and the fold silently skips them. `sealed()` is the
    /// one predicate every lane's fold is gated on — and it must keep saying
    /// YES for a rejected-only height, which sealed a block that just did
    /// nothing.
    #[test]
    fn a_discarded_batch_never_seals_a_height_to_fold() {
        let blobs = BlobHandle::default();
        let frames = vec![
            // the whole batch at height 20 was discarded by the ceiling.
            drained(1, 20, node::Disposition::Discarded, 1, None),
            drained(2, 20, node::Disposition::Discarded, 1, None),
            // an idle nop height: rejected, no row, but a block DID seal here.
            drained(
                3,
                21,
                node::Disposition::Rejected,
                2,
                Some(node::DrainedOp {
                    origin: Origin::External(vec![9]),
                    target: NOP_TARGET.into(),
                    payload: Vec::new(),
                    dispatches: Vec::new(),
                    latency_us: 0,
                }),
            ),
        ];

        let projections = project_block(&frames, Vec::new(), &blobs);

        let sealed: Vec<(u64, bool)> = projections.iter().map(|p| (p.height, p.sealed())).collect();
        assert_eq!(
            sealed,
            vec![(20, false), (21, true)],
            "a discarded-only height sealed nothing and must not fold; a \
             rejected-only height sealed a block and must"
        );
        assert!(
            projections[0].record.is_none() && projections[0].dispatches.is_empty(),
            "a discarded height carries no row and no dispatches either — \
             folding it would write the watermark key alone"
        );
    }
}
