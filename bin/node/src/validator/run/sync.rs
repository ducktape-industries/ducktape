//! State-sync request handling over loop-owned consensus state.

use super::ValidatorRuntime;
use crate::host_reads::{read_sync_mesh_window, read_valset_members, read_valset_residents};
use crate::sync::serve::{SyncBoundary, SyncIndexOps, SyncStateRequest};
use crate::util::{participant_bytes, resident_bytes};

/// up to [`crate::sync::serve::INDEX_OPS_LOOP_PAGES`] wire pages of a module's
/// stored op rows at or below `up_to_height`, in key order, for the joiner
/// backfill lane. the store's own `scan` already pages in key order off one
/// MVCC snapshot; all this adds is the height ceiling — op keys are
/// fixed-width hex, so lexicographic order IS `(height, seq)` order and the
/// ceiling is a prefix of the page.
///
/// READING A BUDGET, not a page: this read crosses to the consensus loop, and
/// the serve task hands the surplus out itself, so a joiner's walk costs one
/// touch per budget instead of one per wire page.
pub(crate) fn read_index_ops(
    index: &indexer::IndexStore,
    module: &str,
    after: Option<(u64, u32)>,
    up_to_height: u64,
) -> Result<SyncIndexOps, String> {
    let cursor = after.map(|(height, seq)| indexer::op_key(height, seq));
    let page = index
        .scan(
            module,
            indexer::OP_PREFIX.as_bytes(),
            cursor.as_deref().map(str::as_bytes),
            // a whole budget of wire pages, so the store's own `has_more`
            // answers for all of them and nothing is dropped downstream.
            crate::sync::serve::INDEX_OPS_LOOP_ROWS,
        )
        .map_err(|e| format!("index op page for {module}: {e}"))?;
    let above_ceiling =
        |key: &[u8]| indexer::parse_op_key(key).is_none_or(|(height, _)| height > up_to_height);
    let scanned = page.entries.len();
    let served = page
        .entries
        .iter()
        .position(|(key, _)| above_ceiling(key))
        .unwrap_or(scanned);
    let mut rows = Vec::with_capacity(served);
    for (key, value) in page.entries.into_iter().take(served) {
        // op keys are ascii by construction; anything else means a damaged
        // store, and shipping it would only damage the joiner's too.
        let key = String::from_utf8(key)
            .map_err(|_| format!("index op key for {module} is not utf-8 — rebuild the index"))?;
        rows.push((key, value));
    }
    Ok(SyncIndexOps {
        rows,
        // reaching the ceiling ENDS the walk; only a page cut short by the
        // store's own limit still owes rows.
        has_more: served == scanned && page.has_more,
        source_floor: index
            .vouched_floor(module)
            .map_err(|e| format!("index floor for {module}: {e}"))?,
        applied_height: index
            .applied_height(module)
            .map_err(|e| format!("index watermark for {module}: {e}"))?,
    })
}

impl ValidatorRuntime<'_> {
    pub(super) async fn on_sync(&mut self, req: SyncStateRequest) {
        let Self {
            node,
            orchestrator,
            latest_floor,
            index,
            ..
        } = self;

        // the statesync serve task's state touches (the
        // [`SyncStateRequest`] seam): each is one bounded read
        // against loop-owned state — the heavy serving (decode,
        // captures, slicing, replies) lives on the serve task.
        match req {
            SyncStateRequest::Boundary { known, reply } => {
                // the boundary's consensus coordinates ride the manifest.
                // the floor certificate is served only when it certifies
                // exactly the current boundary — a cert behind the
                // boundary would make a joiner skip history it needs.
                // the mesh window rides COMMITTED valset state (the same
                // read point as Standing), deliberately not the
                // epoch-frozen orchestrator sets: a frozen set at a live
                // generation index would recreate the joiner/member
                // asymmetry the window exists to end.
                let (generation, mesh_window) = read_sync_mesh_window(node.host()).await;
                let coords = statesync::BoundaryCoords {
                    epoch: orchestrator.epoch(),
                    view_base: orchestrator.epoch_base(),
                    participants: participant_bytes(orchestrator),
                    residents: resident_bytes(orchestrator),
                    floor_cert: latest_floor
                        .as_ref()
                        .filter(|fc| fc.epoch == orchestrator.epoch())
                        .filter(|fc| node.finalized().is_some_and(|f| f.height == fc.height))
                        .map(|fc| fc.cert.clone()),
                    generation,
                    mesh_window,
                    // the replay guard a joiner inherits, cut at the boundary
                    // it will seat on: an entry above that height names a
                    // batch the joiner has not reached and must still apply.
                    applied_frames: match node.finalized() {
                        Some(f) => node
                            .replay_window()
                            .into_iter()
                            .filter(|(height, _)| *height <= f.height)
                            .collect(),
                        None => Vec::new(),
                    },
                    // this orchestrator's own armed cutover, if any — a seat
                    // built from this boundary resumes with the SAME ceiling
                    // instead of observing the already-changed valset itself
                    // and arming a later one (#1821).
                    pending_cutover_view: orchestrator
                        .pending_cutover()
                        .map(|cutover| cutover.cutover_view()),
                };
                let finalized_for_sync = node
                    .finalized()
                    .filter(|f| f.height <= coords.view_base || coords.floor_cert.is_some());
                let answer = match finalized_for_sync {
                    // two refusals, named apart: no boundary at
                    // all (pre-first-block), vs the per-block
                    // window where the tip advanced but its
                    // finalization certificate has not persisted
                    // yet — a retry lands once they align.
                    None => Err(match node.finalized() {
                        Some(f) => format!(
                            "boundary {} awaiting its finalization certificate — \
                                     retry",
                            f.height
                        ),
                        None => "no finalized boundary to serve yet".to_string(),
                    }),
                    Some(finalized) => {
                        let id = statesync::BoundaryId {
                            height: finalized.height,
                            root_hash: finalized.root_hash,
                        };
                        if known.contains(&id) {
                            // the serve task holds this boundary's
                            // payload — coordinates only.
                            Ok(SyncBoundary {
                                id,
                                coords,
                                data: None,
                            })
                        } else {
                            statesync::capture_boundary(node.host(), finalized, &coords)
                                .await
                                .map(|(id, data)| SyncBoundary {
                                    id,
                                    coords,
                                    data: Some(data),
                                })
                        }
                    }
                };
                let _ = reply.send(answer);
            }
            SyncStateRequest::ModuleServe {
                module_id,
                body,
                reply,
            } => {
                let served = node
                    .host()
                    .serve_sync(&module_id, &body)
                    .await
                    .map_err(|e| format!("module {module_id} serve_sync: {e}"));
                let _ = reply.send(served);
            }
            SyncStateRequest::Frames {
                after_height,
                up_to_height,
                reply,
            } => {
                // the decode is a bounded seek-and-read since #1814, but it
                // is still real journal I/O — spawned onto its own task so a
                // slow disk stalls a joiner's reply, never this loop's next
                // `select_biased!` arm (drain, `/v1/query`, ...). the handle
                // is self-contained (its own `Arc`'d journal and a snapshot
                // of the height index), so nothing here holds `node` past
                // this match arm.
                match node.sink_mut().frame_reader().await {
                    Ok(reader) => {
                        tokio::spawn(async move {
                            let read = reader
                                .read_finalized_frames(
                                    after_height,
                                    up_to_height,
                                    statesync::FRAME_BATCH_LEN,
                                )
                                .await;
                            let _ = reply.send(read);
                        });
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            SyncStateRequest::IndexOps {
                module,
                after,
                up_to_height,
                reply,
            } => {
                let _ = reply.send(read_index_ops(index, &module, after, up_to_height));
            }
            SyncStateRequest::TipCoords { reply } => {
                // the detection lane: everything here is already
                // loop-owned state — no capture, and deliberately
                // no floor-cert alignment gate. that gate protects
                // a JOINER from syncing a boundary whose history
                // it would skip; a detection reply carries a
                // presence bit, never certificate bytes, and every
                // action taken on it (ascension, promotion)
                // re-fetches a full manifest through the gated
                // Boundary path.
                let answer = match node.finalized() {
                    None => Err("no finalized boundary to serve yet".to_string()),
                    Some(f) => {
                        // committed valset reads, like the Standing arm — the
                        // window must reflect a just-committed grant NOW, not
                        // at the epoch cutover.
                        let (generation, mesh_window) = read_sync_mesh_window(node.host()).await;
                        Ok(statesync::TipCoords {
                            height: f.height,
                            root_hash: f.root_hash,
                            epoch: orchestrator.epoch(),
                            view_base: orchestrator.epoch_base(),
                            participants: participant_bytes(orchestrator),
                            residents: resident_bytes(orchestrator),
                            has_floor: latest_floor
                                .as_ref()
                                .filter(|fc| fc.epoch == orchestrator.epoch())
                                .is_some_and(|fc| fc.height == f.height),
                            generation,
                            mesh_window,
                            // the detection lane's only DIAGNOSTIC: this
                            // node's build stamp, so a poller can name a
                            // skew instead of watching roots drift in
                            // silence. `None` for a build that cannot
                            // identify itself — a silence, not a claim —
                            // and nothing on either side gates on it.
                            build: noded::services::build_identity().map(str::to_string),
                        })
                    }
                };
                let _ = reply.send(answer);
            }
            SyncStateRequest::Standing { requester, reply } => {
                // the fail-closed standing gate. read the COMMITTED
                // valset projection (updates at the Redeem block, unlike the
                // orchestrator's transport set which lags to the cutover), so a
                // freshly-admitted resident is servable the instant its grant
                // commits. these are local in-memory host queries on the loop's
                // own state — the same read point `read_valset_residents` uses
                // elsewhere, between drains, no deadlock.
                let host = node.host();
                let members = read_valset_members(host).await.unwrap_or_default();
                let residents = read_valset_residents(host).await;
                let standing = members
                    .iter()
                    .chain(residents.iter())
                    .any(|k| k.as_slice() == requester);
                let _ = reply.send(standing);
            }
        }
    }
}
