use super::envelope::RUNNER_RESULT_MARKER;
use super::{AgentResponse, Deserialize, Serialize};

/// the fixed value of the `ducktape_delivery` magic key on the faceted
/// job-finalize payload — part of the wrapper's self-identifying token,
/// never a version.
const DELIVERY_RECEIPT_MARKER: u32 = 1;

/// the wrapper the oracle's provisioning path returns instead of the bare
/// response text: the model prose plus the host-assembled facets — message
/// (`response_text`) / artifact (`workspace_receipt`) / sink / status.
/// `deny_unknown_fields`: the assembled shape is this crate's own contract
/// with compute-service, and an unrecognized key is drift, not forward compat
/// — the retired `data`/`effects` facets are now rejected here, not tolerated.
/// `sink`/`status` keep `#[serde(default)]` because the oracle SKIP-SERIALIZES
/// them when empty/default (the minimal
/// `{ducktape_runner_result, response_text, workspace_receipt}` shape) —
/// load-bearing, not forward-compat. the single delivery path
/// ([`RunsModule::deliver_run_result`]) applies whatever facets are present.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerResult {
    pub(super) ducktape_runner_result: u32,
    pub(super) response_text: String,
    /// Native event-only execution: not an empty model response fallback.
    #[serde(default)]
    pub(super) native_input_handled: bool,
    #[serde(default)]
    pub(super) native_cancelled: bool,
    pub(super) workspace_receipt: WorkspaceReceipt,
    /// O1/O2 output sink; default [`WireSink::Chain`].
    #[serde(default)]
    pub(super) sink: WireSink,
    /// the host's terminal observation; default [`WireStatus::Ok`].
    #[serde(default)]
    pub(super) status: WireStatus,
}

/// field-for-field mirror of compute-service's assembled receipt.
/// `deny_unknown_fields`: the receipt is this crate's contract with the
/// assembler, and a key this build does not know is skew, never something to
/// tolerate — the mirrors must stay provably identical.
#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceReceipt {
    pub(crate) source_prefix: String,
    /// the pinned SOURCE snapshot echoed from the envelope (contract §5 keeps
    /// it on the receipt). decode-validated only: delivery derives everything
    /// from the OUTPUT coordinates.
    #[allow(dead_code)]
    pub(crate) source_snapshot: Option<String>,
    pub(crate) output_snapshot: Option<String>,
    pub(crate) commit_height: Option<u64>,
    pub(crate) rebased: bool,
    pub(crate) no_changes: bool,
    /// `Some` iff the executor's workspace commit failed — the writes were not
    /// captured (paired with a `degraded` status by the wrapper). the oracle
    /// skip-serializes it on healthy receipts (a missing Option key is None).
    pub(crate) commit_error: Option<String>,
    /// forge (§5): the pushed work branch — `Some` on a pushed receipt, and
    /// the ATTEMPTED branch on a commit-failed one; absent on every duckfs
    /// receipt.
    pub(crate) branch: Option<String>,
    /// forge (§5): the new commit oid — the forge output_ref is
    /// `branch@output_commit`. `Some` only when a push landed.
    pub(crate) output_commit: Option<String>,
}

/// the receipt's durable output reference for the delivered-runs ring: the
/// forge `branch@output_commit` when a push landed, else the duckfs output
/// snapshot, else `None` (nothing moved this run).
pub(super) fn output_ref_of(receipt: &WorkspaceReceipt) -> Option<String> {
    match (&receipt.branch, &receipt.output_commit) {
        (Some(branch), Some(oid)) => Some(format!("{branch}@{oid}")),
        _ => receipt.output_snapshot.clone(),
    }
}

/// the O1/O2 output sink. internally tagged on `mode`; a MISSING sink field
/// defaults to `Chain` via the `#[serde(default)]` on [`RunnerResult::sink`],
/// and a present `{"mode":"pr",...}` decodes to `Pr`.
///
/// ONE shape, both directions (M1): the same type also SERIALIZES as the
/// envelope's REQUESTED sink (`result_contract.sink`). a requested `Pr`
/// carries empty title/body — the keys skip-serialize, and delivery derives
/// them from the message facet — and `Chain` composes as an absent field
/// (see [`WireSink::is_chain`]), matching the oracle's own skip.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum WireSink {
    #[default]
    Chain,
    Pr {
        #[serde(default)]
        repo: String,
        source_branch: String,
        #[serde(default)]
        target_branch: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
    },
}

impl WireSink {
    /// the composed `result_contract` omits the sink key entirely for `Chain`
    /// — the serde skip mirroring compute-service's `is_chain`.
    pub(crate) fn is_chain(&self) -> bool {
        matches!(self, WireSink::Chain)
    }

    /// is `self` the SAME COMMITMENT as `committed` — the binding
    /// [`crate::response`]'s delivery enforces (#1835): same mode, and for
    /// `Pr` the same repo/source/target. `title`/`body` are provider-authored
    /// prose delivery always derives fresh and never trusts from either side
    /// (see `emit_sink`'s doc comment), so they never enter this comparison —
    /// an echo that only embellishes the PR's title is not a redirect.
    pub(crate) fn same_commitment(&self, committed: &WireSink) -> bool {
        match (self, committed) {
            (WireSink::Chain, WireSink::Chain) => true,
            (
                WireSink::Pr {
                    repo: r1,
                    source_branch: s1,
                    target_branch: t1,
                    ..
                },
                WireSink::Pr {
                    repo: r2,
                    source_branch: s2,
                    target_branch: t2,
                    ..
                },
            ) => r1 == r2 && s1 == s2 && t1 == t2,
            _ => false,
        }
    }
}

/// the host's terminal observation of a run. `Failed` fails the run even with a
/// present message facet; `Degraded` still delivers (surfaced in the receipt).
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum WireStatus {
    #[default]
    Ok,
    Degraded,
    Failed,
}

// ---- faceted delivery --------------------------------------------------------
// the single delivery path decodes the runner result below and applies whatever
// facets it carries; a plain (message-only) result carries none.

/// decode the full faceted [`RunnerResult`]. marker strict (R4): a result
/// that is not a well-formed `ducktape_runner_result` wrapper FAILS THE RUN,
/// deterministically — the flat-string passthrough for marker-less bytes is
/// gone (the oracle always wraps).
pub(super) fn decode_run_result(bytes: &[u8]) -> Result<RunnerResult, String> {
    let result: RunnerResult = serde_json::from_slice(bytes)
        .map_err(|e| format!("runner result is malformed (no flat-payload tolerance): {e}"))?;
    if result.ducktape_runner_result != RUNNER_RESULT_MARKER {
        return Err(format!(
            "runner result marker {} is not the ducktape_runner_result magic \
             ({RUNNER_RESULT_MARKER})",
            result.ducktape_runner_result
        ));
    }
    Ok(result)
}

/// the faceted job-finalize payload: the validated response, the derived
/// output_ref (O1), and the status. deterministic — fixed serde field order.
#[derive(Serialize)]
struct DeliveryReceipt<'a> {
    ducktape_delivery: u32,
    response: &'a AgentResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_ref: Option<OutputRef<'a>>,
    status: &'static str,
}

/// the artifact facet distilled to a chainable reference (O1): a downstream run
/// can set `workspace.source = prior.output_snapshot`; a forge run's output is
/// the git coordinates `branch@output_commit` instead (the snapshot key stays a
/// stated `null`, the forge keys skip-serialize on duckfs receipts — pre-forge
/// bytes are unchanged).
#[derive(Serialize, Clone)]
struct OutputRef<'a> {
    source_prefix: &'a str,
    output_snapshot: Option<&'a str>,
    commit_height: Option<u64>,
    rebased: bool,
    no_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_commit: Option<&'a str>,
}

pub(super) fn encode_delivery_receipt(
    response: &AgentResponse,
    receipt: &WorkspaceReceipt,
    status: WireStatus,
) -> String {
    // an output exists when the run produced a duckfs snapshot OR pushed a
    // forge commit; both distill into the one output_ref shape.
    let output_ref =
        (receipt.output_snapshot.is_some() || receipt.output_commit.is_some()).then(|| OutputRef {
            source_prefix: &receipt.source_prefix,
            output_snapshot: receipt.output_snapshot.as_deref(),
            commit_height: receipt.commit_height,
            rebased: receipt.rebased,
            no_changes: receipt.no_changes,
            branch: receipt.branch.as_deref(),
            output_commit: receipt.output_commit.as_deref(),
        });
    let status = match status {
        WireStatus::Ok => "ok",
        WireStatus::Degraded => "degraded",
        WireStatus::Failed => "failed",
    };
    // the finalize payload MUST stay valid JSON within the jobs cap (the naive
    // byte-truncation the jobs board applies would corrupt it). the response is
    // capped by validation (MAX_REPLY_BLOCKS_BYTES + MAX_ACTIONS_BYTES) and
    // output_ref/status are tiny, so this receipt always fits by construction.
    serde_json::to_string(&DeliveryReceipt {
        ducktape_delivery: DELIVERY_RECEIPT_MARKER,
        response,
        output_ref,
        status,
    })
    .expect("delivery receipt serializes")
}
