//! the derived-index tier: shared store construction (each module's index
//! guest installed from the network's genesis, or from a founding set for a
//! daemon that runs no network), boundary stamping, and the `/v1/index/*` +
//! `/v1/blocks` snapshot read lane.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::{NodeHandle, error_response, hex_bytes};

/// how many recent blocks `GET /v1/blocks` serves when the caller names no
/// `limit` — a bounded default page over the durable block index.
const BLOCKS_DEFAULT_LIMIT: usize = 256;

// ---------------------------------------------------------------------------
// the derived-index tier: shared construction. noded and the consensus
// validator (bin/node) both reuse this router, so they share the store setup
// too — one construction site, identical /v1/index/* behavior, each binary
// passing its own genesis module list.
// ---------------------------------------------------------------------------

/// open the per-module index store under `<storage>/index`, deciding nothing
/// about guests yet: each database keeps serving the guest its last converge
/// installed until [`converge_host_modules`] runs. The split is what lets a
/// node bring its index routes up before it holds a genesis (a joiner
/// fetches its genesis off the mesh after the surfaces start). an open
/// failure is fatal-with-remedy for the caller, and there are exactly two
/// remedies: a tier nobody holds is rebuildable, so "delete the directory" —
/// but a tier another process HOLDS is a healthy one owned by a running
/// node, and deleting it would take the derived tier out from under a live
/// validator. Every caller of this function reaches both cases (the node's
/// own boot races a second node; `node qualify` runs against a workspace
/// whose node was never stopped), so the split is here and not in any of them.
pub fn open_index_store<S: AsRef<str>>(
    storage: &std::path::Path,
    module_ids: &[S],
) -> Result<Arc<indexer::IndexStore>, String> {
    let index_dir = storage.join("index");
    let ids: Vec<&str> = module_ids.iter().map(AsRef::as_ref).collect();
    indexer::IndexStore::open_bare(&index_dir, &ids)
        .map(Arc::new)
        .map_err(|err| match err.is_store_locked() {
            true => format!(
                "a node is already running on this workspace and holds the module index at {} \
                 — stop that node first (^C the `ducktape node run`, or stop the unit that \
                 supervises it)",
                index_dir.display()
            ),
            false => format!(
                "open module index at {}: {err} (derived tier — delete the directory to rebuild)",
                index_dir.display()
            ),
        })
}

/// Install the mapper belonging to each running deployment. Called after
/// composition and before a sealed block reaches the derived tier, so genesis,
/// live admission, code replacement and recovery all use the same path.
///
/// A converge failure the index store itself scoped to one module (a
/// rejected mapper — `index.is_poisoned()` still reads false) is logged and
/// skipped: that module's own reads and writes stay refused until it
/// reconverges, but every OTHER module — this loop's remaining entries, and
/// the block the caller applies right after this returns — must keep
/// indexing. Only a converge failure that poisoned the whole store (the
/// engine itself, not the candidate bytes) aborts here.
pub fn converge_host_modules(index: &indexer::IndexStore, host: &host::Host) -> Result<(), String> {
    let modules: Vec<indexer::IndexModule<'_>> = host
        .module_index_guests()
        .map(|(id, guest)| indexer::IndexModule { id, guest })
        .collect();
    index_host_modules(index, modules.iter().map(|module| module.id))?;
    for module in modules {
        // `converge`/`converge_deployment` below may move `module` (built
        // into a one-element slice), so its id — and whether it was ALREADY
        // failed before this attempt — are captured first for the log line
        // that can follow the match.
        let module_id = module.id;
        let was_failed = index.is_module_failed(module_id);
        let result = match host.module_code_hash(module_id) {
            Some(hash) => index.converge_deployment(&module, &hash),
            None => index.converge(&[module]),
        };
        let Err(error) = result else { continue };
        if index.is_poisoned() {
            return Err(format!("converge deployed index guests: {error}"));
        }
        // this loop runs every block for as long as a rejected mapper stays
        // deployed (converge_host_modules is called per fold, not per
        // deployment change), so `warn` fires ONLY on the transition into
        // `Failed` — a repeat of the identical rejection is `debug`, per the
        // "no info/warn more than once per block" logging rule.
        if was_failed {
            tracing::debug!(
                target: "ducktape::index",
                reason = "module_mapper_still_rejected",
                module = module_id,
                error = %error,
                "module's index guest is still rejected, unchanged since the last attempt"
            );
            continue;
        }
        tracing::warn!(
            target: "ducktape::index",
            reason = "module_mapper_rejected",
            module = module_id,
            error = %error,
            "module's index guest failed to converge — its fold and views stay \
             refused until it reconverges; every other module keeps indexing"
        );
    }
    Ok(())
}

/// the index covers every module the host runs: open a database for each id
/// of `modules` the store lacks — a module the modules registry admitted
/// after the store opened — leaving the ones it holds untouched. idempotent,
/// and free when nothing was admitted. called after every compose and before
/// every block fold, so an admitted module's feed begins at the block that
/// seated it. The database opens bare; `converge_host_modules` installs the
/// optional mapper from the running deployment before feeding that block.
pub fn index_host_modules<'a>(
    index: &indexer::IndexStore,
    modules: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    for id in modules {
        index.open_module(id).map_err(|err| {
            format!(
                "open index database for module {id} at {}: {err} \
                 (derived tier — delete the directory to rebuild)",
                index.base().display()
            )
        })?;
    }
    Ok(())
}

/// flatten a dispatch origin into the index's plain origin tag: external
/// submitter identities render via [`indexer::user_handle`] — printable claimed
/// names pass through, raw ed25519 pubkeys become hex (never the lossy `�`
/// boxes a plain utf-8 decode leaves). the feed carries origins pre-rendered,
/// so index guests never see raw key bytes. hex-keyed identity belongs to the
/// explorer row's `proposer`, not the index op rows.
pub fn index_origin(origin: &sdk::Origin) -> indexer::OriginTag {
    match origin {
        sdk::Origin::External(id) => indexer::OriginTag::external(indexer::user_handle(id)),
        sdk::Origin::Module(id) => indexer::OriginTag::module(id.clone()),
        sdk::Origin::Program(account) => indexer::OriginTag::program(*account),
        sdk::Origin::System => indexer::OriginTag::system(),
    }
}

/// one sealed block's dispatch trace as the indexer's fold input. `time` is
/// the block's consensus time — noded passes its submit context's clock, the
/// consensus validator stamps `consensus_time = height`. an empty trace is a
/// real block (a rejected frame consumed its height): folding it advances
/// every module's watermark so staleness checks stay exact.
///
/// `record` starts [`None`]: a caller holding a block the explorer shows
/// grafts its [`block_row`] on via struct update. the live drain builds it
/// from the decoded frame; the validator's boot folds (journal replay, frame
/// catch-up) rebuild the SAME row from the sealed frame bytes riding the
/// replay observer — the row is not reproducible from the dispatch trace
/// alone, so a fold without frame content leaves it `None`.
pub fn index_block_ops(
    height: u64,
    time: u64,
    dispatches: &[host::DispatchRecord],
) -> indexer::BlockOps {
    indexer::BlockOps {
        height,
        time,
        ops: dispatches
            .iter()
            .map(|d| indexer::AppliedOp {
                origin: index_origin(&d.origin),
                module: d.module.clone(),
                payload: d.payload.clone(),
                assigned: d.assigned.clone(),
            })
            .collect(),
        record: None,
    }
}

/// stamp every module whose watermark trails `boundary` as backfilled: its
/// feed and views honestly BEGIN at the boundary, visibly via the floor
/// (`/v1/index/status` reports it). call wherever canonical state advanced
/// without the op stream — after state-sync installs a boundary, after
/// recovery skipped re-executing durable blocks, or over a wiped index
/// directory. history below a boundary re-enters only by replaying blocks
/// through the feed, or by the joiner's op-row backfill pulling the source's
/// stored rows in below the stamp (indexable spec §7). returns the stamped ids.
pub fn stamp_stale_modules(
    index: &indexer::IndexStore,
    boundary: u64,
) -> Result<Vec<String>, indexer::Error> {
    let stale = stale_modules(index, boundary)?;
    for module in &stale {
        index.mark_backfilled(module, boundary)?;
    }
    Ok(stale)
}

/// every module whose op feed trails `boundary` — the stamp's candidates,
/// listed WITHOUT stamping them. a caller that can pull the missing rows
/// decides per module whether the feed is resumable (extend it and keep the
/// views under it) or has to be stamped and rebuilt from the boundary down.
pub fn stale_modules(
    index: &indexer::IndexStore,
    boundary: u64,
) -> Result<Vec<String>, indexer::Error> {
    let modules = index.module_ids();
    let mut stale = Vec::new();
    for module in modules {
        if index.applied_height(&module)? >= boundary {
            continue;
        }
        stale.push(module);
    }
    Ok(stale)
}

// ---------------------------------------------------------------------------
// the derived-index read lane. like the blob lane these
// handlers never cross the actor: the store is `Send + Sync` and every read
// runs at its own MVCC snapshot, concurrent with the actor's block writes.
// ---------------------------------------------------------------------------

/// query params for `GET /v1/index/{module}/scan` and `…/ops`.
#[derive(Debug, Deserialize)]
pub struct IndexScanParams {
    /// key prefix to scan under. ignored by `…/ops` (pinned to the op log).
    pub prefix: Option<String>,
    /// opaque page cursor: the `next_after` of the previous page.
    pub after: Option<String>,
    /// page size; the store clamps oversized asks.
    pub limit: Option<usize>,
}

/// default page size when a client sends no `limit`.
const INDEX_DEFAULT_LIMIT: usize = 100;

/// one scanned entry. values written by this tier are json (`value`); a
/// derived value that is not valid json ships as `value_hex` instead.
#[derive(Serialize)]
struct IndexEntry {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_hex: Option<String>,
}

#[derive(Serialize)]
struct IndexScanResponse {
    entries: Vec<IndexEntry>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<String>,
}

#[derive(Serialize)]
struct IndexOpsResponse {
    /// op rows projected to json (height/seq/time/origin + payload). the
    /// stored envelope is borsh (the guest feed); this lane re-presents it
    /// in the row shape the tier always served: a payload that is valid
    /// json embeds verbatim as `payload`, anything else ships as
    /// `payload_hex` — the codebase's hex-not-base64 convention.
    ops: Vec<serde_json::Value>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<String>,
}

/// hard cap on how many payload bytes [`op_row_json`] will hex- or json-embed
/// inline (#1809): ordinary duckfs traffic puts megabyte stage chunks in op
/// payloads, and hex doubles that. a payload over this ships as
/// `payload_bytes` + `payload_truncated` instead — the full bytes stay
/// reachable by op id, this row just stops being the way to fetch them.
const MAX_INLINE_PAYLOAD_BYTES: usize = 64 * 1024;

/// one borsh op row as this lane's json projection.
fn op_row_json(row: &indexer::OpRow) -> serde_json::Value {
    let mut out = serde_json::json!({
        "height": row.height,
        "seq": row.seq,
        "time": row.time,
        "origin": row.origin,
    });
    if row.payload.len() > MAX_INLINE_PAYLOAD_BYTES {
        out["payload_bytes"] = serde_json::Value::from(row.payload.len());
        out["payload_truncated"] = serde_json::Value::Bool(true);
    } else {
        let payload_json: Option<serde_json::Value> = serde_json::from_slice(&row.payload).ok();
        match payload_json {
            Some(value) => out["payload"] = value,
            None => out["payload_hex"] = serde_json::Value::String(hex_bytes(&row.payload)),
        }
    }
    if !row.assigned.is_empty() {
        let assigned_json: Option<serde_json::Value> = serde_json::from_slice(&row.assigned).ok();
        match assigned_json {
            Some(value) => out["assigned"] = value,
            None => out["assigned_hex"] = serde_json::Value::String(hex_bytes(&row.assigned)),
        }
    }
    out
}

fn index_store(handle: &NodeHandle) -> Option<&Arc<indexer::IndexStore>> {
    handle.index.as_ref()
}

fn no_index_store_response() -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "no index store configured")
}

fn index_error(err: indexer::Error) -> Response {
    let status = match err {
        indexer::Error::UnknownModule(_) | indexer::Error::ViewUnsupported => StatusCode::NOT_FOUND,
        indexer::Error::View(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &err.to_string())
}

/// run `work` on `spawn_blocking`'s pool — the one place every `Lane::Open`
/// index read (`index_status`, `index_view`, `index_ops`, `index_scan`,
/// `blocks`) gets off the axum worker. a panicked blocking task answers 500
/// instead of dropping the connection silently.
async fn off_worker<F, T>(work: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Ok(value),
        Err(_) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "index task panicked",
        )),
    }
}

/// GET /v1/index/status — each module's applied watermark (the op FEED, not
/// the derived view), the poison flag, backfill floors, and every fold
/// trigger's health (`pending` backlog + last drain error): the watermark
/// vouches for the feed, the fold trails it observably. a poisoned index
/// keeps serving (stale but consistent) reads; the remedy is a rebuild,
/// which this surface makes visible. boundary-stamped modules also report
/// their backfill floor: content below it was never in the feed — the gap
/// stays visible instead of papered over.
///
/// `fold_status` per module costs fluent31 an iteration over that trigger's
/// whole pending-queue range (no cheap counter exists — see the "ASKING IS
/// NOT FREE" note on `IndexStore::fold_status`), so a backlogged module makes
/// this call as expensive as [`index_view`]'s wasm query, so the sampling
/// loop runs off the axum worker on [`off_worker`] like every other read on
/// this surface.
pub(crate) async fn index_status(State(handle): State<NodeHandle>) -> Response {
    let Some(store) = index_store(&handle).cloned() else {
        return no_index_store_response();
    };
    match off_worker(move || index_status_body(&store)).await {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(response)) => *response,
        Err(response) => response,
    }
}

/// the synchronous per-module scan behind [`index_status`], split out so it
/// can run on `spawn_blocking`'s pool: three `Result`-returning store reads
/// per module, any of which can name the response outright as an early
/// `Err`, so this returns a `Response` on the error path rather than
/// threading `indexer::Error` back through a `?` the caller would have to
/// re-translate. `Response` boxed on the error path — clippy's
/// `result_large_err`, since a `Response` dwarfs the `Ok` payload.
fn index_status_body(store: &indexer::IndexStore) -> Result<serde_json::Value, Box<Response>> {
    let mut modules = serde_json::Map::new();
    let mut backfilled = serde_json::Map::new();
    let mut fold = serde_json::Map::new();
    for id in store.module_ids() {
        match store.applied_height(&id) {
            Ok(height) => {
                modules.insert(id.to_string(), height.into());
            }
            Err(err) => return Err(Box::new(index_error(err))),
        }
        match store.backfill_height(&id) {
            Ok(Some(floor)) => {
                backfilled.insert(id.to_string(), floor.into());
            }
            Ok(None) => {}
            Err(err) => return Err(Box::new(index_error(err))),
        }
        match store.fold_status(&id) {
            Ok(Some(status)) => match serde_json::to_value(&status) {
                Ok(value) => {
                    fold.insert(id.to_string(), value);
                }
                Err(_) => {
                    return Err(Box::new(error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "fold status did not serialize",
                    )));
                }
            },
            Ok(None) => {}
            Err(err) => return Err(Box::new(index_error(err))),
        }
    }
    Ok(serde_json::json!({
        "poisoned": store.is_poisoned(),
        "modules": modules,
        "backfilled": backfilled,
        "fold": fold,
    }))
}

/// GET /v1/index/{module}/ops?after=&limit= — one page of the module's op
/// log, oldest-first. rows are the stored envelopes verbatim; page forward by
/// echoing `next_after` as the next call's `after`. the synchronous fluent31
/// scan runs on [`off_worker`] like `index_view`/`index_status`.
pub(crate) async fn index_ops(
    State(handle): State<NodeHandle>,
    Path(module): Path<String>,
    Query(params): Query<IndexScanParams>,
) -> Response {
    let Some(store) = index_store(&handle).cloned() else {
        return no_index_store_response();
    };
    let after = params.after.clone();
    let limit = params.limit.unwrap_or(INDEX_DEFAULT_LIMIT);
    let outcome = off_worker(move || {
        store.scan(
            &module,
            indexer::OP_PREFIX.as_bytes(),
            after.as_deref().map(str::as_bytes),
            limit,
        )
    })
    .await;
    let page = match outcome {
        Ok(Ok(page)) => page,
        Ok(Err(err)) => return index_error(err),
        Err(response) => return response,
    };
    let mut ops = Vec::with_capacity(page.entries.len());
    for (_key, value) in &page.entries {
        match borsh::from_slice::<indexer::OpRow>(value) {
            Ok(row) => ops.push(op_row_json(&row)),
            // rows are written as borsh by construction; failing one means the
            // store is damaged — say so instead of silently dropping it.
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "stored op row was not a borsh envelope — rebuild the index",
                );
            }
        }
    }
    Json(IndexOpsResponse {
        ops,
        has_more: page.has_more,
        next_after: page.next_after,
    })
    .into_response()
}

/// the fold watermark a view reply carries: `"{height}:{seq}"`, the op row the
/// module's fold had consumed when the reply was served. ABSENT when the
/// module has no tip (fresh database, boundary stamp, a guest reinstalled
/// without a refold) — absent means unknown, never zero.
///
/// a HEADER, not an envelope: the module reply enums are the modules' own wire
/// and stay untouched, and a caller that does not care never sees it.
pub const FOLDED_HEADER: &str = "x-ducktape-folded";

/// POST /v1/index/{module}/view — the module's materialized view, served by
/// its registered mapper. request body and reply are module-defined json
/// (chat: `{"search": {…}}` → `{"hits": […]}`), exactly as opaque to the
/// daemon as `/v1/query` payloads are. modules with no view answer 404 —
/// some never will (forge's substrate is already a queryable git repo).
///
/// the reply carries [`FOLDED_HEADER`]: how far the fold had consumed the op
/// feed, so a caller that just wrote can tell whether this snapshot contains
/// its own op. it answers read-after-YOUR-OWN-WRITE and nothing else — the
/// fold advances only on op traffic, so a quiet module's tip is arbitrarily
/// old while its view is perfectly current.
pub(crate) async fn index_view(
    State(handle): State<NodeHandle>,
    Path(module): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let Some(store) = index_store(&handle).cloned() else {
        return no_index_store_response();
    };
    let req_bytes = serde_json::to_vec(&req).expect("a decoded json value re-serializes");
    let query_module = module.clone();
    // One deployment guard covers watermark and view; the synchronous read
    // runs off the worker.
    let outcome = off_worker(move || store.view_with_tip(&query_module, &req_bytes)).await;
    let indexer::IndexedView { bytes, folded } = match outcome {
        Ok(Ok(view)) => view,
        Ok(Err(error)) => return index_error(error),
        Err(response) => return response,
    };
    let mut response = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => Json(value).into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "view reply was not json"),
    };
    if let Some((height, seq)) = folded {
        // infallible on purpose: ABSENT is the one honest way to say "no tip"
        // (§3.2.4), so a header dropped for any other reason would read to the
        // caller as an unstamped module. two integers and a colon are visible
        // ascii, which is exactly what a header value may hold.
        let watermark = HeaderValue::from_str(&format!("{height}:{seq}"))
            .expect("a numeric watermark is a valid header value");
        response.headers_mut().insert(FOLDED_HEADER, watermark);
    }
    response
}

/// GET /v1/index/{module}/scan?prefix=&after=&limit= — one page of raw index
/// keys, for a module's derived read models (everything a registered
/// `ModuleIndexer` materialized outside the reserved op/meta spaces).
pub(crate) async fn index_scan(
    State(handle): State<NodeHandle>,
    Path(module): Path<String>,
    Query(params): Query<IndexScanParams>,
) -> Response {
    let Some(store) = index_store(&handle).cloned() else {
        return no_index_store_response();
    };
    let prefix = params.prefix.unwrap_or_default();
    let after = params.after.clone();
    let limit = params.limit.unwrap_or(INDEX_DEFAULT_LIMIT);
    let outcome = off_worker(move || {
        store.scan(
            &module,
            prefix.as_bytes(),
            after.as_deref().map(str::as_bytes),
            limit,
        )
    })
    .await;
    let page = match outcome {
        Ok(Ok(page)) => page,
        Ok(Err(err)) => return index_error(err),
        Err(response) => return response,
    };
    let entries = page
        .entries
        .iter()
        .map(|(key, value)| {
            let json: Option<Box<serde_json::value::RawValue>> = serde_json::from_slice(value).ok();
            IndexEntry {
                key: String::from_utf8_lossy(key).into_owned(),
                value_hex: json.is_none().then(|| hex_bytes(value)),
                value: json,
            }
        })
        .collect();
    Json(IndexScanResponse {
        entries,
        has_more: page.has_more,
        next_after: page.next_after,
    })
    .into_response()
}

/// query params for `GET /v1/blocks`.
#[derive(Debug, Deserialize)]
pub struct BlocksParams {
    /// cap the response to the most recent N blocks (default:
    /// [`BLOCKS_DEFAULT_LIMIT`]).
    pub limit: Option<usize>,
}

/// GET /v1/blocks — the recent rows this node kept, oldest-first:
/// `{"blocks":[…]}`.
///
/// reads the index store's durable blocks database directly (no actor
/// round-trip), the same discipline as the other `/v1/index/*` reads — so
/// history survives a restart. heartbeat nops never get a row, so an empty
/// reply means no real ops have finalized, not an idle chain. a handle with
/// no index store configured serves the same "no blocks yet" shape.
///
/// NOT uniformly non-empty, and a reader that assumes so is wrong: the block
/// writers drop an op-less block, but [`indexer::IndexStore::apply_block_record`]
/// exists for a follower that observes BOUNDARIES rather than sealed frames,
/// and `bin/node`'s `boundary_block_row` writes one such row (empty `hash`,
/// empty `ops`) at each ascension tip. it is a truthful record of what that
/// node saw; a consumer that presents these rows as blocks-with-content owes
/// its own filter.
///
/// `recent_block_rows` runs on [`off_worker`] like every other synchronous
/// store read on this `Lane::Open` surface.
pub(crate) async fn blocks(
    State(handle): State<NodeHandle>,
    Query(params): Query<BlocksParams>,
) -> Response {
    let Some(store) = handle.index.clone() else {
        return Json(serde_json::json!({ "blocks": [] })).into_response();
    };
    let limit = params.limit.unwrap_or(BLOCKS_DEFAULT_LIMIT);
    let outcome = off_worker(move || store.recent_block_rows(limit)).await;
    let rows = match outcome {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => return index_error(err),
        Err(response) => return response,
    };
    let mut blocks: Vec<Box<serde_json::value::RawValue>> = Vec::with_capacity(rows.len());
    for row in &rows {
        match serde_json::from_slice(row) {
            Ok(block) => blocks.push(block),
            // rows are written as json by construction; failing one means the
            // store is damaged — say so instead of silently dropping it.
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "stored block row was not json — rebuild the index",
                );
            }
        }
    }
    Json(serde_json::json!({ "blocks": blocks })).into_response()
}

#[cfg(test)]
mod tests {
    /// The one condition `node qualify` documents — "the node must be
    /// STOPPED" — is the one its refusal used to mis-name: the locked index
    /// took the same "delete the directory to rebuild" clause as a damaged
    /// one, and an operator who follows it deletes the derived tier of a
    /// LIVE validator. A held lock says a node owns this workspace; the only
    /// remedy is to stop it.
    ///
    /// `flock` is per open file description, so a second open in THIS process
    /// conflicts exactly as another process's would.
    #[test]
    fn a_locked_index_names_the_running_node_and_never_offers_the_delete() {
        let storage = tempfile::tempdir().expect("scratch storage");
        let _held = super::open_index_store::<&str>(storage.path(), &[]).expect("first open owns");
        let refused = super::open_index_store::<&str>(storage.path(), &[])
            .err()
            .expect("a second open cannot take the lock");
        assert!(
            refused.contains("a node is already running on this workspace"),
            "{refused}"
        );
        assert!(refused.contains("stop that node first"), "{refused}");
        assert!(
            !refused.contains("delete the directory"),
            "the delete remedy is for a tier nobody holds: {refused}"
        );
    }

    /// #1809: a payload over [`super::MAX_INLINE_PAYLOAD_BYTES`] ships as
    /// `payload_bytes` + `payload_truncated` instead of a multi-megabyte hex
    /// string — the full bytes stay reachable by op id, this row just stops
    /// hexing them.
    #[test]
    fn op_row_json_truncates_an_oversized_payload() {
        let row = indexer::OpRow {
            height: 1,
            seq: 0,
            time: 1_000,
            origin: indexer::OriginTag::external("jess"),
            payload: vec![0u8; super::MAX_INLINE_PAYLOAD_BYTES + 1],
            assigned: Vec::new(),
        };
        let json = super::op_row_json(&row);
        assert_eq!(
            json["payload_bytes"],
            serde_json::json!(super::MAX_INLINE_PAYLOAD_BYTES + 1)
        );
        assert_eq!(json["payload_truncated"], serde_json::json!(true));
        assert!(json.get("payload").is_none());
        assert!(json.get("payload_hex").is_none());
    }

    /// an in-budget payload keeps embedding verbatim — the truncation branch
    /// must not swallow ordinary rows.
    #[test]
    fn op_row_json_embeds_a_small_payload_verbatim() {
        let row = indexer::OpRow {
            height: 1,
            seq: 0,
            time: 1_000,
            origin: indexer::OriginTag::external("jess"),
            payload: br#"{"hello":"world"}"#.to_vec(),
            assigned: Vec::new(),
        };
        let json = super::op_row_json(&row);
        assert_eq!(json["payload"], serde_json::json!({"hello": "world"}));
        assert!(json.get("payload_bytes").is_none());
        assert!(json.get("payload_truncated").is_none());
    }
}
