use host::Host;
use recovery::{Manifest, Recovery};
use statesync::{fetch_frames_capped, fetch_manifest};

use crate::blob_fetch::SourceRotate;
use crate::constants::CUTOVER_DELAY;
use crate::explorer::IndexFold;
use crate::sync::serve::{TrustAnchor, to_node_disposition, verify_manifest_floor};
use crate::util::hex;

/// height span a single catch-up window covers before its frames are applied
/// and dropped — bounds the suffix fold's working set to one window
/// regardless of how far behind the source's (unverified) tip claims to be.
const CATCHUP_WINDOW_HEIGHTS: u64 = statesync::MAX_APPLIED_FRAMES as u64;

pub(crate) async fn apply_verified_suffix_frame(
    host: &mut Host,
    served: &statesync::FinalizedFrame,
    prepared: host::PreparedWork,
    code_source: &dyn host::CodeSource,
    witness: &mut dyn host::CommitWitness,
) -> Result<Vec<host::DispatchRecord>, String> {
    let expected = to_node_disposition(served.disposition);
    // CODE-SWAP REALIZATION, mirroring the live drain and recovery replay: a
    // frame sealed after a code-registry swap executed on the NEW component, so
    // catch-up must swap before re-applying or the served roots cannot
    // reproduce. fail-closed on missing/tampered bytes.
    host.realize_module_swaps(served.height, code_source)
        .await
        .map_err(|e| format!("code-swap realization at height {}: {e}", served.height))?;
    // the served frame is a BATCH: decode its members and apply as ONE block,
    // exactly like the live drain and recovery replay, so the disposition,
    // roots, and root-hash reproduce what the peer served. disposition is
    // DRAIN-based (any member applied, or a System injection ran), never
    // root-hash-based.
    let (outcome, dispatches) = match node::decode_batch(&served.frame) {
        Ok(members) => {
            let mut ops = Vec::new();
            for member in &members {
                if let Ok(op) = node::decode_member(member) {
                    ops.push(op);
                }
            }
            let ctx = host::BlockContext {
                height: served.height,
                consensus_time: served.height,
                origin: sdk::Origin::System,
            };
            // the served frame carries no journal of the peer's internal work:
            // this node's committed state is the network's at this height (its
            // roots verified), so the same units prepare here identically and
            // their authority decides live on all-pre state.
            match host
                .submit_block_prepared(ctx, ops, prepared, witness)
                .await
            {
                Ok(batch) => {
                    let (ran, dispatches) = batch.into_trace();
                    let outcome = if ran {
                        node::Disposition::Applied
                    } else {
                        node::Disposition::Rejected
                    };
                    (outcome, dispatches)
                }
                Err(host::SubmitError::Rejected(_)) => (node::Disposition::Rejected, Vec::new()),
                Err(host::SubmitError::Fatal(f)) => {
                    return Err(format!("fatal host error applying suffix frame: {f}"));
                }
            }
        }
        Err(_) => (node::Disposition::Rejected, Vec::new()),
    };
    if outcome != expected {
        return Err(format!(
            "served seal mismatch at height {}: replay landed as {outcome:?}, \
             served as {expected:?}",
            served.height
        ));
    }
    let roots = host.module_roots();
    if roots != served.roots {
        return Err(format!(
            "served seal mismatch at height {}: roots changed to {:?}, served {:?}",
            served.height, roots, served.roots
        ));
    }
    let root_hash = host.root_hash();
    if root_hash != served.root_hash {
        return Err(format!(
            "served seal mismatch at height {}: root_hash {} != served {}",
            served.height,
            hex(&root_hash),
            hex(&served.root_hash)
        ));
    }
    Ok(dispatches)
}
pub(crate) async fn apply_and_journal_verified_frame<E>(
    recovery: &mut Recovery<E>,
    host: &mut Host,
    frame: &statesync::FinalizedFrame,
    fold: Option<&mut IndexFold<'_>>,
) -> Result<(), String>
where
    E: recovery::Context + commonware_runtime::BufferPooler + commonware_runtime::Supervisor,
{
    // realize swaps through the SAME source replay uses (wired on Recovery), so
    // every path reconciles code identically.
    let code_source = recovery.code_source();
    let prepared = host
        .prepare_work(frame.height)
        .await
        .map_err(|e| format!("catch-up prepare at height {}: {e}", frame.height))?;
    let expected = node::BlockSeal {
        height: frame.height,
        disposition: to_node_disposition(frame.disposition),
        roots: frame.roots.clone(),
        root_hash: frame.root_hash,
    };
    recovery
        .pre_apply_catchup(&frame.frame, &prepared, &expected)
        .await
        .map_err(|e| format!("catch-up WAL write: {e}"))?;
    let dispatches =
        apply_verified_suffix_frame(host, frame, prepared, code_source.as_ref(), recovery).await?;
    let seal = node::BlockSeal {
        height: frame.height,
        disposition: to_node_disposition(frame.disposition),
        roots: host.module_roots(),
        root_hash: host.root_hash(),
    };
    node::BlockSink::seal(recovery, &seal)
        .await
        .map_err(|e| format!("catch-up seal write: {e}"))?;
    if let Some(fold) = fold {
        use recovery::ReplaySink as _;
        fold.folded_block(&recovery::FoldedBlock {
            host,
            height: frame.height,
            frame: &frame.frame,
            disposition: seal.disposition,
            root_hash: seal.root_hash,
            dispatches: &dispatches,
            roots: &seal.roots,
        });
    }
    Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct SuffixCatchupApply {
    pub(crate) applied: usize,
}

/// apply one fetched window's frames, then hand each frame's bytes to `store`
/// as it lands — the window's bytes never accumulate past this call, so a
/// full-run backlog never holds more than one window's worth at a time.
pub(crate) async fn apply_suffix_frames<E>(
    recovery: &mut Recovery<E>,
    host: &mut Host,
    from_height: u64,
    to_height: u64,
    frames: Vec<statesync::FinalizedFrame>,
    mut fold: Option<&mut IndexFold<'_>>,
    store: &consensus::ContentStore,
) -> Result<SuffixCatchupApply, String>
where
    E: recovery::Context + commonware_runtime::BufferPooler + commonware_runtime::Supervisor,
{
    if to_height < from_height {
        return Err(format!(
            "invalid catch-up range ({from_height}, {to_height}]"
        ));
    }
    if from_height == to_height {
        if !frames.is_empty() {
            return Err(format!(
                "no-gap catch-up received {} unexpected frames",
                frames.len()
            ));
        }
        return Ok(SuffixCatchupApply::default());
    }
    if frames.last().map(|f| f.height) != Some(to_height) {
        return Err(format!(
            "catch-up frames stopped before target height {to_height}"
        ));
    }

    let mut last = from_height;
    let mut applied = SuffixCatchupApply::default();
    for frame in frames {
        // STRICTLY INCREASING, in range: a real height gap is not itself
        // evidence of an omitted block — the cutover ceiling (`node`'s
        // `OrderedNode::view_ceiling`) discards straggler views on every
        // honest node, so a legitimate suffix can skip a height. What
        // catches a source that omits (or invents) a block it actually
        // holds is the root-hash cross-check below, over the frames it DID
        // serve, against the tip this run already anchored.
        if frame.height <= last || frame.height > to_height {
            return Err(format!(
                "catch-up frame height {} outside ({last}, {to_height}]",
                frame.height
            ));
        }
        apply_and_journal_verified_frame(recovery, host, &frame, fold.as_deref_mut()).await?;
        last = frame.height;
        applied.applied += 1;
        store.put(frame.frame);
    }
    Ok(applied)
}

#[derive(Debug)]
pub(crate) struct SuffixCatchup {
    pub(crate) to_height: u64,
}

#[derive(Debug)]
pub(crate) enum SuffixCatchupError {
    Retry(String),
    Fatal(String),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn catch_up_suffix_frames<C, E>(
    client: &C,
    recovery: &mut Recovery<E>,
    host: &mut Host,
    fold: Option<&mut IndexFold<'_>>,
    recovered_height: u64,
    max_iterations: usize,
    namespace: &[u8],
    anchor: TrustAnchor<'_>,
    store: &consensus::ContentStore,
) -> Result<SuffixCatchup, SuffixCatchupError>
where
    C: statesync::SyncClient + SourceRotate,
    E: recovery::Context + commonware_runtime::BufferPooler + commonware_runtime::Supervisor,
{
    let mut fold = fold;
    let mut current_height = recovered_height;
    let mut total_frames = 0usize;

    for _ in 0..=max_iterations {
        let tip = fetch_manifest(client).await.map_err(|e| {
            SuffixCatchupError::Retry(format!("catch-up manifest unavailable: {e}"))
        })?;
        // THE TRUST GATE: the tip's height, root_hash, and floor are the
        // SOURCE's own claim about itself — verify_manifest_floor ties its
        // participant set back to this node's anchor and checks a real
        // quorum signed its floor, exactly like the boundary this replica
        // already bootstrapped from. a tip that fails this is not a frame
        // shortage worth retrying against the SAME source; rotate away.
        if let Err(e) = verify_manifest_floor(namespace, anchor, &tip) {
            client.rotate_source();
            return Err(SuffixCatchupError::Retry(format!(
                "catch-up tip manifest unanchored: {e}"
            )));
        }
        if tip.height <= current_height {
            if tip.height == current_height && host.root_hash() != tip.root_hash {
                return Err(SuffixCatchupError::Fatal(format!(
                    "catch-up source hash {} at height {} does not match recovered host {}",
                    hex(&tip.root_hash),
                    tip.height,
                    hex(&host.root_hash())
                )));
            }
            tracing::debug!(
                target: "ducktape::statesync",
                from = recovered_height,
                to = current_height,
                frames = total_frames,
                "suffix catch-up planned"
            );
            return Ok(SuffixCatchup {
                to_height: current_height,
            });
        }

        // WINDOWED: apply and discard each fetched span before asking for the
        // next one, so the suffix fold's working set never grows past one
        // window regardless of how far behind the (unverified, but now
        // anchored) tip claims to be.
        while current_height < tip.height {
            let window_to = current_height
                .saturating_add(CATCHUP_WINDOW_HEIGHTS)
                .min(tip.height);
            let frames = match fetch_frames_capped(
                client,
                current_height,
                window_to,
                statesync::MAX_CATCHUP_BYTES,
            )
            .await
            {
                Ok(frames) => frames,
                Err(statesync::SyncError::RangePruned {
                    requested_after,
                    retained_from,
                }) => {
                    // the follower side of the same wedge (#493, macOS "missing blocks").
                    // it printed the SAME impossible range on every certificate, which is
                    // indistinguishable from healthy catch-up — and so it read as boot
                    // noise for days. `permanent` is the word that ends the guessing: this
                    // does not heal by waiting, because the source can only prune FURTHER
                    // ahead of us.
                    tracing::error!(
                        target: "ducktape::statesync",
                        requested_after,
                        retained_from,
                        gap_blocks = retained_from.saturating_sub(requested_after),
                        permanent = true,
                        "catch-up IMPOSSIBLE — the source pruned past our height; waiting will \
                         never fix this, we must full-sync from a fresh checkpoint"
                    );
                    return Err(SuffixCatchupError::Retry(format!(
                        "source pruned past requested height {requested_after}; retained from \
                         {retained_from}"
                    )));
                }
                Err(e) => {
                    return Err(SuffixCatchupError::Retry(format!(
                        "catch-up frame suffix unavailable: {e}"
                    )));
                }
            };
            let applied = apply_suffix_frames(
                recovery,
                host,
                current_height,
                window_to,
                frames,
                fold.as_deref_mut(),
                store,
            )
            .await
            .map_err(SuffixCatchupError::Fatal)?;
            current_height = window_to;
            total_frames += applied.applied;
        }
        if host.root_hash() != tip.root_hash {
            return Err(SuffixCatchupError::Fatal(format!(
                "catch-up frames landed at {}, target manifest {}",
                hex(&host.root_hash()),
                hex(&tip.root_hash)
            )));
        }
    }

    tracing::debug!(
        target: "ducktape::statesync",
        from = recovered_height,
        to = current_height,
        frames = total_frames,
        "suffix catch-up planned"
    );
    Ok(SuffixCatchup {
        to_height: current_height,
    })
}

pub(crate) fn advance_next_seq_from_frames(next_seq: &mut u64, frames: &[Vec<u8>], me: &[u8]) {
    for frame in frames {
        // Both pinned proposals and finalized journal records contain batches.
        let Ok(members) = node::decode_batch(frame) else {
            continue;
        };
        for member in members {
            if let Some((origin, seq)) = node::frame_origin_seq(&member)
                && origin == me
            {
                // Finalized batches are journaled before member admission;
                // their claimed sender and sequence are not yet authenticated.
                if node::decode_frame(&member).is_err() {
                    continue;
                }
                let following = seq.checked_add(1).expect("local submit sequence exhausted");
                *next_seq = (*next_seq).max(following);
            }
        }
    }
}

pub(crate) fn derive_pending_boot(manifest: &Manifest, rec: &recovery::Recovered) -> Option<u64> {
    let checkpoint_pending = if rec.epoch == manifest.epoch {
        manifest.pending_cutover_view
    } else {
        None
    };
    checkpoint_pending.or_else(|| {
        let mut prev_root = manifest.root("valset").expect("valset is a genesis module");
        let mut armed = None;
        for (height, roots) in &rec.blocks {
            let root = roots
                .iter()
                .find(|(id, _)| id == "valset")
                .map(|(_, r)| *r)
                .expect("every seal carries the full root vector");
            if root != prev_root && *height > rec.view_base && armed.is_none() {
                armed = Some(*height - rec.view_base + CUTOVER_DELAY);
            }
            prev_root = root;
        }
        armed
    })
}

#[cfg(test)]
mod tests {
    use consensus::{ObservationOutcome, ValsetOrchestrator};
    use sdk::StateRoot;

    use super::*;

    const VALSET_ROOT: StateRoot = StateRoot([9; sdk::ROOT_LEN]);

    #[test]
    fn recovered_batches_advance_only_the_local_submit_sequence() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let local = PrivateKey::from_seed(1);
        let foreign = PrivateKey::from_seed(2);
        let message = sdk::Msg {
            target: "heartbeat".into(),
            payload: Vec::new(),
        };
        let retained = vec![
            node::encode_batch(&[node::encode_frame(&local, 41, &message)]),
            node::encode_batch(&[
                node::encode_frame(&foreign, 900, &message),
                node::encode_frame(&local, 47, &message),
                node::encode_frame(&local, 43, &message),
            ]),
        ];
        let mut next_seq = 40;
        advance_next_seq_from_frames(&mut next_seq, &retained, local.public_key().as_ref());
        assert_eq!(next_seq, 48);

        // A checkpoint already ahead of retained proposals must never regress.
        next_seq = 70;
        advance_next_seq_from_frames(&mut next_seq, &retained, local.public_key().as_ref());
        assert_eq!(next_seq, 70);
    }

    #[test]
    fn sequence_recovery_ignores_malformed_batches_and_members() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let local = PrivateKey::from_seed(1);
        let message = sdk::Msg {
            target: "heartbeat".into(),
            payload: Vec::new(),
        };
        let valid = node::encode_frame(&local, 12, &message);
        let retained = vec![
            vec![0xff],
            node::encode_batch(&[]),
            node::encode_batch(&[Vec::new(), valid]),
        ];
        let mut next_seq = 3;
        advance_next_seq_from_frames(&mut next_seq, &retained, local.public_key().as_ref());
        assert_eq!(next_seq, 13);
    }

    #[test]
    fn sequence_recovery_does_not_trust_forged_local_headers() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let local = PrivateKey::from_seed(1);
        let message = sdk::Msg {
            target: "heartbeat".into(),
            payload: Vec::new(),
        };
        let mut forged = node::frame_preimage(
            keyscheme::KeyScheme::Ed25519,
            local.public_key().as_ref(),
            u64::MAX,
            &message,
        );
        let mut truncated = forged.clone();
        truncated.pop();
        forged.extend_from_slice(&[0; 64]);

        for member in [forged, truncated] {
            assert_eq!(
                node::frame_origin_seq(&member),
                Some((local.public_key().as_ref().to_vec(), u64::MAX))
            );
            assert!(node::decode_frame(&member).is_err());
            let mut next_seq = 40;
            advance_next_seq_from_frames(
                &mut next_seq,
                &[node::encode_batch(&[member])],
                local.public_key().as_ref(),
            );
            assert_eq!(next_seq, 40);
        }
    }

    #[test]
    #[should_panic(expected = "local submit sequence exhausted")]
    fn sequence_recovery_fails_closed_when_the_local_sequence_is_exhausted() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let local = PrivateKey::from_seed(1);
        let member = node::encode_frame(
            &local,
            u64::MAX,
            &sdk::Msg {
                target: "heartbeat".into(),
                payload: Vec::new(),
            },
        );
        advance_next_seq_from_frames(
            &mut 40,
            &[node::encode_batch(&[member])],
            local.public_key().as_ref(),
        );
    }

    /// the drain's checkpoint capture, with the manifest fields this path
    /// reads and defaults for the rest.
    fn manifest(height: u64, pending_cutover_view: Option<u64>) -> Manifest {
        Manifest {
            height: Some(height),
            epoch: 0,
            view_base: 0,
            participants: vec![vec![1], vec![2]],
            residents: Vec::new(),
            pending_cutover_view,
            root_hash: StateRoot([0; sdk::ROOT_LEN]),
            // the change is already IN the checkpointed valset root: it is the
            // block at `height` that moved it.
            roots: vec![("valset".to_string(), VALSET_ROOT)],
            codes: Vec::new(),
            snapshots: Vec::new(),
            oplog_pos: 0,
            next_seq: 0,
            applied_frames: Vec::new(),
        }
    }

    /// what the journal retained above that checkpoint when the node died a
    /// few block times later: sealed heights whose valset root never moves
    /// again, because the move already happened AT the checkpoint.
    fn recovered_above(manifest: &Manifest, heights: &[u64]) -> recovery::Recovered {
        recovery::Recovered {
            height: heights.last().copied().or(manifest.height),
            root_hash: manifest.root_hash,
            epoch: manifest.epoch,
            view_base: manifest.view_base,
            participants: manifest.participants.clone(),
            residents: manifest.residents.clone(),
            frames: Vec::new(),
            blocks: heights
                .iter()
                .map(|h| (*h, vec![("valset".to_string(), VALSET_ROOT)]))
                .collect(),
            applied_frames: Vec::new(),
            applied: 0,
            skipped: 0,
            rolled_forward: false,
        }
    }

    /// THE CHECKPOINT MUST CARRY THE CUTOVER THE SAME DRAIN PASS ARMED.
    ///
    /// A valset change lands at engine view V and the checkpoint cadence fires
    /// on that same pass. The manifest's height IS V's height — the
    /// observation barrier makes that alignment exact, not unlikely — so the
    /// re-arm scan on the next boot can never see the change: it reads only
    /// blocks ABOVE the manifest height, seeded with the manifest's
    /// already-changed valset root. `pending_cutover_view` is the ONLY re-arm
    /// path in this case, and it is only truthful if the checkpoint is
    /// captured AFTER the drain's orchestration step.
    #[test]
    fn a_drain_checkpoint_at_the_changing_view_re_arms_the_ceiling_peers_use() {
        let changing_view = 40;
        let mut orchestrator = ValsetOrchestrator::new(CUTOVER_DELAY, [vec![1u8], vec![2]]);
        let ObservationOutcome::Scheduled(armed) =
            orchestrator.observe_members(changing_view, [vec![1u8], vec![2], vec![3]], [])
        else {
            panic!("a membership change schedules a cutover");
        };
        // every peer that observed the same block armed exactly this view.
        let peers_ceiling = changing_view + CUTOVER_DELAY;
        assert_eq!(armed.cutover_view(), peers_ceiling);

        // the checkpoint, captured after the orchestration step.
        let captured = manifest(
            changing_view,
            orchestrator.pending_cutover().map(|c| c.cutover_view()),
        );
        let rec = recovered_above(&captured, &[changing_view + 1]);

        let pending_boot = derive_pending_boot(&captured, &rec);
        assert_eq!(
            pending_boot,
            Some(peers_ceiling),
            "the restart must re-arm the ceiling its peers are converging on"
        );
        let resumed = ValsetOrchestrator::resume(
            CUTOVER_DELAY,
            rec.participants.clone(),
            rec.residents.clone(),
            rec.epoch,
            rec.view_base,
            pending_boot,
        );
        assert_eq!(
            resumed.pending_cutover().map(|c| c.cutover_view()),
            Some(peers_ceiling)
        );

        // ...and the pre-fix capture point, for the record: a manifest written
        // before the pass armed the cutover leaves NOTHING to recover it from.
        // The node then applies the views its peers discarded and cuts over
        // one view late — a silent fork.
        assert_eq!(
            derive_pending_boot(&manifest(changing_view, None), &rec),
            None,
            "the scan cannot see a change that happened at the manifest height"
        );
    }
}
