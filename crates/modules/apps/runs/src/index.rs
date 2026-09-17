//! runs' read model: the run journal — every lifecycle fact the module
//! committed about a run, in commit order, and the run's current state as
//! the fold of them — folded from the applied-op feed into runs' per-module
//! index database.
//!
//! canonical runs state keeps only what consensus needs: the in-flight
//! correlation entries, the live sessions, and a short delivered-runs ring.
//! the lifecycle itself — who dispatched a run, which node picked it up,
//! what it did, how it ended — is what an operator watches, and it lives
//! here, where the engine iterates natively and nothing is pruned.
//!
//! the feed carries the facts as the module's own assigned stamp
//! ([`crate::RunEvent`], [`crate::decode_assigned`]): an op that moved a run
//! stamps the exact transitions it committed, so the fold never re-derives a
//! transition from a payload. an op with an empty stamp moved no run.
//!
//! key spaces (inside runs' per-module index database):
//! - `run/{dispatch_id}` — one [`RunView`] per run, its current state.
//! - `journal/{dispatch_id}/{height:016x}/{seq:08x}/{n:04x}` — one
//!   [`JournalRow`] per fact, in commit order: the op's position in the
//!   chain, then the fact's position within the op.
//! - `recent/{!height:016x}/{!seq:08x}/{!n:04x}` — the dispatch id of every
//!   run, newest dispatch first (the counters are bitwise inverted so an
//!   ascending scan reads newest-first).
//! - `agent/{agent_id}/{!height:016x}/{!seq:08x}/{!n:04x}` — the same, per
//!   agent.
//!
//! the dispatch id (hex sha256 of the run id, [`crate::dispatch_id_for`]) is
//! the key segment because a run id carries the run separator and
//! caller-chosen text; the digest is fixed-width and separator-free.
//!
//! this file is the DECISION core — pure functions over [`StateRead`],
//! compiled natively and unit-tested against a plain map. the wasm shell
//! (`src/index_guest.rs`, feature `index-guest`) wires it into the engine.

use std::collections::BTreeMap;

use index_guest::{Fail, MAX_SCAN_LIMIT, OpRow, StateRead, Writes};
use serde::{Deserialize, Serialize};

use crate::{
    OP_AGENT_CALL, OP_CHAT_POST_MESSAGE, OP_COLLABORATION_ACKNOWLEDGE, OP_COLLABORATION_DELIVER,
    OP_DUCKFS_WRITE_TEXT, OP_JOBS_COMMENT, OP_MODULES_UPDATE, OP_PAGES_COMMENT, OP_PAGES_POST,
    OP_PAGES_SET_CHECKED, OP_REPLY, OP_TASKS_CREATE, OP_TASKS_UPDATE_STATUS, PageSource, PrRef,
    RunEvent, RunFact, RunOutcome, decode_assigned, delegated_run_id_for, dispatch_id_for,
    page_source,
};
// the shapes this fold answers with live in the wire crate, so the daemon can
// decode a `/v1/index/runs` reply without linking the module that folded it.
// re-exported here, so `runs::index::RunsViewQuery` still names the same type.
pub use runs_wire::view::*;

/// [`Fail`] code: an applied op's assigned stamp did not decode — interface
/// drift, which only a refold can honestly repair.
const FAIL_ASSIGNED_DECODE: i32 = 2;
/// [`Fail`] code: a stored row did not decode — a damaged read model.
const FAIL_ROW_DECODE: i32 = 3;
/// [`Fail`] code: a view request this mapper does not speak.
const FAIL_BAD_REQUEST: i32 = 4;


/// the place a run was called from, read off its dispatch fact. A delegated
/// run has none: its caller is a run, and the journal keys that edge by its
/// delegation id rather than a place.
fn origin_place(channel_id: &str, anchor_seq: u64, job_id: &Option<String>) -> Option<RunPlace> {
    if let Some(job_id) = job_id {
        return Some(RunPlace::Job {
            job_id: job_id.clone(),
        });
    }
    match page_source(channel_id) {
        Some(PageSource::Block(block_id)) => {
            return Some(RunPlace::PageBlock {
                block_id: block_id.into(),
            });
        }
        Some(PageSource::CommentThread(thread_id)) => {
            return Some(RunPlace::PageThread {
                thread_id: thread_id.into(),
            });
        }
        None => {}
    }
    if let Some(item) = crate::forge_source::parse_forge_channel(channel_id) {
        return Some(RunPlace::ForgeItem {
            repo: item.repo.into(),
            number: item.number,
        });
    }
    let has_anchor = !channel_id.is_empty() && anchor_seq != 0;
    has_anchor.then(|| RunPlace::ChatMessage {
        channel_id: channel_id.into(),
        seq: anchor_seq,
    })
}

/// a chat destination as its receipts name it: under a thread root, or at
/// the channel's top level.
fn chat_place(channel_id: String, thread: Option<u64>) -> RunPlace {
    match thread {
        Some(seq) => RunPlace::ChatMessage { channel_id, seq },
        None => RunPlace::Channel { channel_id },
    }
}

/// the receipt shapes the catalog's operations mint, decoded by operation
/// name. `reply` nests the destination it resolved; every explicit operation
/// reports its coordinates flat.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplyDestinationReceipt {
    Chat {
        channel_id: String,
        #[serde(default)]
        thread: Option<u64>,
    },
    Page {
        target: String,
    },
    PageThread {
        thread_id: String,
    },
    Job {
        job_id: String,
    },
}

#[derive(Deserialize)]
struct ReplyReceipt {
    destination: ReplyDestinationReceipt,
}

#[derive(Deserialize)]
struct ChatPostReceipt {
    channel_id: String,
    #[serde(default)]
    thread: Option<u64>,
}

#[derive(Deserialize)]
struct PageCommentReceipt {
    #[serde(default)]
    target: String,
    #[serde(default)]
    thread_id: String,
}

#[derive(Deserialize)]
struct BlockReceipt {
    block_id: String,
}

#[derive(Deserialize)]
struct PageReceipt {
    page_id: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct JobReceipt {
    job_id: String,
}

#[derive(Deserialize)]
struct TaskReceipt {
    task_id: String,
}

#[derive(Deserialize)]
struct FileReceipt {
    path: String,
}

#[derive(Deserialize)]
struct ModuleReceipt {
    module_id: String,
}

#[derive(Deserialize)]
struct ChannelReceipt {
    channel_id: String,
}

#[derive(Deserialize)]
struct DelegationReceipt {
    delegation_id: String,
    callee_agent_id: String,
}

fn receipt<T: for<'de> Deserialize<'de>>(result: &serde_json::Value) -> Option<T> {
    serde_json::from_value(result.clone()).ok()
}

/// the place one staged action landed on, from the receipt its preparer
/// minted. A reaction names its anchor, which the origin already does; an
/// effect with no receipt (the forge sink's label) names nothing here — its
/// PR arrives as its own fact once authenticated.
fn acted_place(operation: &str, result: &serde_json::Value) -> Option<RunPlace> {
    match operation {
        OP_REPLY => {
            let ReplyReceipt { destination } = receipt(result)?;
            Some(match destination {
                ReplyDestinationReceipt::Chat { channel_id, thread } => {
                    chat_place(channel_id, thread)
                }
                ReplyDestinationReceipt::Page { target } => {
                    RunPlace::PageBlock { block_id: target }
                }
                ReplyDestinationReceipt::PageThread { thread_id } => {
                    RunPlace::PageThread { thread_id }
                }
                ReplyDestinationReceipt::Job { job_id } => RunPlace::Job { job_id },
            })
        }
        OP_CHAT_POST_MESSAGE => {
            let ChatPostReceipt { channel_id, thread } = receipt(result)?;
            Some(chat_place(channel_id, thread))
        }
        OP_PAGES_COMMENT => {
            let PageCommentReceipt { target, thread_id } = receipt(result)?;
            let has_block_target = !target.is_empty();
            Some(match has_block_target {
                true => RunPlace::PageBlock { block_id: target },
                false => RunPlace::PageThread { thread_id },
            })
        }
        OP_PAGES_SET_CHECKED => {
            let BlockReceipt { block_id } = receipt(result)?;
            Some(RunPlace::PageBlock { block_id })
        }
        OP_PAGES_POST => {
            let PageReceipt { page_id, title } = receipt(result)?;
            Some(RunPlace::Page { page_id, title })
        }
        OP_JOBS_COMMENT => {
            let JobReceipt { job_id } = receipt(result)?;
            Some(RunPlace::Job { job_id })
        }
        OP_TASKS_CREATE | OP_TASKS_UPDATE_STATUS => {
            let TaskReceipt { task_id } = receipt(result)?;
            Some(RunPlace::Task { task_id })
        }
        OP_DUCKFS_WRITE_TEXT => {
            let FileReceipt { path } = receipt(result)?;
            Some(RunPlace::File { path })
        }
        OP_MODULES_UPDATE => {
            let ModuleReceipt { module_id } = receipt(result)?;
            Some(RunPlace::Module { module_id })
        }
        OP_COLLABORATION_DELIVER | OP_COLLABORATION_ACKNOWLEDGE => {
            let ChannelReceipt { channel_id } = receipt(result)?;
            Some(RunPlace::Channel { channel_id })
        }
        OP_AGENT_CALL => {
            let DelegationReceipt {
                delegation_id,
                callee_agent_id,
            } = receipt(result)?;
            Some(RunPlace::Run {
                dispatch_id: dispatch_id_for(&delegated_run_id_for(
                    &delegation_id,
                    &callee_agent_id,
                )),
            })
        }
        _ => None,
    }
}

fn pr_place(pr: &PrRef) -> RunPlace {
    RunPlace::ForgeItem {
        repo: pr.repo.clone(),
        number: pr.number,
    }
}


fn run_key(dispatch_id: &str) -> String {
    format!("run/{dispatch_id}")
}

fn journal_prefix(dispatch_id: &str) -> String {
    format!("journal/{dispatch_id}/")
}

fn journal_key(dispatch_id: &str, op: &OpRow, n: usize) -> String {
    format!(
        "{}{:016x}/{:08x}/{:04x}",
        journal_prefix(dispatch_id),
        op.height,
        op.seq,
        n
    )
}

/// the newest-first ordinal of a fact: every counter bitwise inverted.
fn newest_first(op: &OpRow, n: usize) -> String {
    format!("{:016x}/{:08x}/{:04x}", !op.height, !op.seq, !(n as u32))
}

const RECENT_PREFIX: &str = "recent/";

fn recent_key(op: &OpRow, n: usize) -> String {
    format!("{RECENT_PREFIX}{}", newest_first(op, n))
}

fn agent_prefix(agent_id: &str) -> String {
    format!("agent/{agent_id}/")
}

fn agent_key(agent_id: &str, op: &OpRow, n: usize) -> String {
    format!("{}{}", agent_prefix(agent_id), newest_first(op, n))
}

fn decode_row<T: for<'de> Deserialize<'de>>(value: &[u8]) -> Result<T, Fail> {
    serde_json::from_slice(value).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn encode_row<T: Serialize>(row: &T) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

/// the run rows this op has already rewritten, read ahead of the store so a
/// second fact about one run in the same op sees the first.
type Touched = BTreeMap<String, RunView>;

fn load_run(
    read: &impl StateRead,
    touched: &Touched,
    dispatch_id: &str,
) -> Result<Option<RunView>, Fail> {
    if let Some(run) = touched.get(dispatch_id) {
        return Ok(Some(run.clone()));
    }
    read.get(run_key(dispatch_id).as_bytes())
        .map(|value| decode_row(&value))
        .transpose()
}

fn stamp(op: &OpRow) -> Stamp {
    Stamp {
        height: op.height,
        time: op.time,
    }
}

/// fold one fact into the run row it is about. a fact about a run this
/// index never saw dispatched (the mapper was installed after the dispatch:
/// no backfill) changes no run row; its journal row is still written.
fn fold_fact(
    read: &impl StateRead,
    touched: &mut Touched,
    out: &mut Writes,
    op: &OpRow,
    n: usize,
    event: &RunEvent,
) -> Result<(), Fail> {
    let dispatch_id = dispatch_id_for(&event.run_id);
    index_guest::put(
        out,
        journal_key(&dispatch_id, op, n),
        encode_row(&JournalRow {
            height: op.height,
            time: op.time,
            fact: event.fact.clone(),
        })?,
    );
    let run = match &event.fact {
        RunFact::Dispatched {
            agent_id,
            channel_id,
            anchor_seq,
            job_id,
            delegation_id,
            requester,
        } => {
            index_guest::put(out, recent_key(op, n), dispatch_id.clone());
            index_guest::put(out, agent_key(agent_id, op, n), dispatch_id.clone());
            RunView {
                run_id: event.run_id.clone(),
                dispatch_id: dispatch_id.clone(),
                agent_id: agent_id.clone(),
                channel_id: channel_id.clone(),
                anchor_seq: *anchor_seq,
                job_id: job_id.clone(),
                delegation_id: delegation_id.clone(),
                requester: requester.clone(),
                dispatched: stamp(op),
                state: RunState::Dispatched,
                actions: 0,
                origin: origin_place(channel_id, *anchor_seq, job_id),
                places: Vec::new(),
            }
        }
        RunFact::SessionOpened { attempt, holder } => {
            let Some(mut run) = load_run(read, touched, &dispatch_id)? else {
                return Ok(());
            };
            run.state = RunState::Running {
                attempt: *attempt,
                holder: holder.clone(),
            };
            run
        }
        RunFact::Acted {
            operation, result, ..
        } => {
            let Some(mut run) = load_run(read, touched, &dispatch_id)? else {
                return Ok(());
            };
            run.actions += 1;
            if let Some(place) = acted_place(operation, result) {
                run.touch(place);
            }
            run
        }
        RunFact::Settled {
            outcome,
            reason,
            degraded,
            executing_node,
            output_ref,
            pr,
        } => {
            let Some(mut run) = load_run(read, touched, &dispatch_id)? else {
                return Ok(());
            };
            run.state = RunState::Settled {
                outcome: *outcome,
                reason: reason.clone(),
                degraded: *degraded,
                executing_node: executing_node.clone(),
                output_ref: output_ref.clone(),
                at: stamp(op),
            };
            if let Some(output_ref) = output_ref {
                run.touch(RunPlace::Output {
                    output_ref: output_ref.clone(),
                });
            }
            if let Some(pr) = pr {
                run.touch(pr_place(pr));
            }
            run
        }
        RunFact::ResultActionRefused { .. } => {
            let Some(mut run) = load_run(read, touched, &dispatch_id)? else {
                return Ok(());
            };
            // the module's own rule: a refusal cannot erase a failure, and a
            // later success cannot erase a refusal.
            if let RunState::Settled {
                outcome: outcome @ RunOutcome::ResultAccepted,
                ..
            } = &mut run.state
            {
                *outcome = RunOutcome::ActionRejected;
            }
            run
        }
        RunFact::PrLinked { pr } => {
            let Some(mut run) = load_run(read, touched, &dispatch_id)? else {
                return Ok(());
            };
            run.touch(pr_place(pr));
            run
        }
    };
    index_guest::put(out, run_key(&dispatch_id), encode_row(&run)?);
    touched.insert(dispatch_id, run);
    Ok(())
}

/// fold one applied op into derived writes: every fact its stamp carries,
/// in order. an applied op passed the module's own validation (a failed op
/// aborts its unit and never reaches the feed), so the fold mirrors the
/// transitions without re-judging them.
pub fn fold_op(op: &OpRow, read: &impl StateRead) -> Result<Writes, Fail> {
    let mut out = Writes::new();
    if op.assigned.is_empty() {
        return Ok(out);
    }
    let journal = decode_assigned(&op.assigned).map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?;
    let mut touched = Touched::new();
    for (n, event) in journal.iter().enumerate() {
        fold_fact(read, &mut touched, &mut out, op, n, event)?;
    }
    Ok(out)
}

/// the runs an ordinal index lists, newest first, resolved to their rows.
fn runs_under(read: &impl StateRead, prefix: &str, limit: usize) -> Result<Vec<RunView>, Fail> {
    let page = read.scan_page(prefix.as_bytes(), None, limit.clamp(1, MAX_SCAN_LIMIT));
    let mut runs = Vec::with_capacity(page.entries.len());
    for (_key, dispatch_id) in &page.entries {
        let dispatch_id = String::from_utf8_lossy(dispatch_id);
        if let Some(run) = load_run(read, &Touched::new(), &dispatch_id)? {
            runs.push(run);
        }
    }
    Ok(runs)
}

/// a run's whole journal in commit order, page after page.
fn journal_of(read: &impl StateRead, dispatch_id: &str) -> Result<Vec<JournalRow>, Fail> {
    let prefix = journal_prefix(dispatch_id);
    let mut rows = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = read.scan_page(prefix.as_bytes(), after.as_deref(), MAX_SCAN_LIMIT);
        for (_key, value) in &page.entries {
            rows.push(decode_row(value)?);
        }
        if !page.has_more {
            return Ok(rows);
        }
        after = page.next_after.map(String::into_bytes);
    }
}

/// serve one materialized-view request.
pub fn serve_view(read: &impl StateRead, req: &[u8]) -> Result<Vec<u8>, Fail> {
    let query: RunsViewQuery =
        serde_json::from_slice(req).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))?;
    let reply = match query {
        RunsViewQuery::Recent { agent_id, limit } => {
            let prefix = match agent_id {
                Some(agent_id) => agent_prefix(&agent_id),
                None => RECENT_PREFIX.to_string(),
            };
            RunsViewReply::Runs(runs_under(read, &prefix, limit.unwrap_or(MAX_SCAN_LIMIT))?)
        }
        RunsViewQuery::Run { dispatch_id } => {
            let detail = match load_run(read, &Touched::new(), &dispatch_id)? {
                Some(run) => Some(Box::new(RunDetail {
                    journal: journal_of(read, &dispatch_id)?,
                    run,
                })),
                None => None,
            };
            RunsViewReply::Run(detail)
        }
    };
    serde_json::to_vec(&reply).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunOrigin, encode_assigned};
    use index_guest::{OriginTag, apply_to_map};

    type Map = BTreeMap<Vec<u8>, Vec<u8>>;

    const RUN: &str = "chat\x1fgeneral\x1f2\x1fbot";
    const OTHER: &str = "chat\x1fgeneral\x1f3\x1fduck";

    fn op(height: u64, seq: u32, journal: &[RunEvent]) -> OpRow {
        OpRow {
            height,
            seq,
            time: height * 10,
            origin: OriginTag::external("node"),
            payload: Vec::new(),
            assigned: match journal.is_empty() {
                true => Vec::new(),
                false => encode_assigned(journal),
            },
        }
    }

    fn event(run_id: &str, fact: RunFact) -> RunEvent {
        RunEvent {
            run_id: run_id.into(),
            fact,
        }
    }

    fn dispatched(agent_id: &str, anchor_seq: u64) -> RunFact {
        RunFact::Dispatched {
            agent_id: agent_id.into(),
            channel_id: "general".into(),
            anchor_seq,
            job_id: None,
            delegation_id: None,
            requester: RunOrigin::External(vec![7; 32]),
        }
    }

    fn settled(outcome: RunOutcome) -> RunFact {
        RunFact::Settled {
            outcome,
            reason: None,
            degraded: false,
            executing_node: "ab".into(),
            output_ref: None,
            pr: None,
        }
    }

    fn acted(request: &str, operation: &str, result: serde_json::Value) -> RunFact {
        RunFact::Acted {
            request_id: request.into(),
            lane: crate::LaneKind::Live,
            operation: operation.into(),
            result,
        }
    }

    fn fold(map: &mut Map, op: &OpRow) {
        let writes = fold_op(op, map).expect("fold");
        apply_to_map(map, writes);
    }

    fn recent(map: &Map, agent_id: Option<&str>) -> Vec<RunView> {
        let req = serde_json::json!({"recent": {"agent_id": agent_id}});
        let reply: RunsViewReply =
            serde_json::from_slice(&serve_view(map, &serde_json::to_vec(&req).unwrap()).unwrap())
                .unwrap();
        match reply {
            RunsViewReply::Runs(runs) => runs,
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    fn detail(map: &Map, run_id: &str) -> Option<RunDetail> {
        let req = serde_json::json!({"run": {"dispatch_id": dispatch_id_for(run_id)}});
        let reply: RunsViewReply =
            serde_json::from_slice(&serve_view(map, &serde_json::to_vec(&req).unwrap()).unwrap())
                .unwrap();
        match reply {
            RunsViewReply::Run(detail) => detail.map(|detail| *detail),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn a_run_folds_from_dispatch_through_settlement_and_keeps_its_journal() {
        let mut map = Map::new();
        fold(&mut map, &op(1, 0, &[event(RUN, dispatched("bot", 2))]));
        let run = &recent(&map, None)[0];
        assert_eq!(run.state, RunState::Dispatched);
        assert_eq!(
            run.dispatched,
            Stamp {
                height: 1,
                time: 10
            }
        );
        assert_eq!(run.dispatch_id, dispatch_id_for(RUN));

        fold(
            &mut map,
            &op(
                2,
                3,
                &[event(
                    RUN,
                    RunFact::SessionOpened {
                        attempt: 0,
                        holder: "ab".into(),
                    },
                )],
            ),
        );
        for (seq, request) in ["r0", "r1"].into_iter().enumerate() {
            fold(
                &mut map,
                &op(
                    3,
                    seq as u32,
                    &[event(
                        RUN,
                        RunFact::Acted {
                            request_id: request.into(),
                            lane: crate::LaneKind::Live,
                            operation: "chat.react".into(),
                            result: serde_json::Value::Null,
                        },
                    )],
                ),
            );
        }
        let run = &recent(&map, None)[0];
        assert_eq!(
            run.state,
            RunState::Running {
                attempt: 0,
                holder: "ab".into()
            }
        );
        assert_eq!(run.actions, 2);

        fold(
            &mut map,
            &op(4, 0, &[event(RUN, settled(RunOutcome::ResultAccepted))]),
        );
        let known = detail(&map, RUN).expect("the run is known");
        let RunState::Settled { outcome, at, .. } = &known.run.state else {
            panic!("settled: {:?}", known.run.state);
        };
        assert_eq!(*outcome, RunOutcome::ResultAccepted);
        assert_eq!(
            *at,
            Stamp {
                height: 4,
                time: 40
            }
        );
        let facts: Vec<(u64, &str)> = known
            .journal
            .iter()
            .map(|row| {
                let name = match &row.fact {
                    RunFact::Dispatched { .. } => "dispatched",
                    RunFact::SessionOpened { .. } => "session_opened",
                    RunFact::Acted { .. } => "acted",
                    RunFact::Settled { .. } => "settled",
                    RunFact::ResultActionRefused { .. } => "result_action_refused",
                    RunFact::PrLinked { .. } => "pr_linked",
                };
                (row.height, name)
            })
            .collect();
        assert_eq!(
            facts,
            [
                (1, "dispatched"),
                (2, "session_opened"),
                (3, "acted"),
                (3, "acted"),
                (4, "settled")
            ]
        );
        assert_eq!(detail(&map, "chat\x1fnowhere\x1f9\x1fbot"), None);
    }

    #[test]
    fn recent_lists_newest_dispatch_first_and_narrows_to_an_agent() {
        let mut map = Map::new();
        fold(&mut map, &op(5, 0, &[event(RUN, dispatched("bot", 2))]));
        // two runs staged by one op keep their in-op order, newest first
        fold(
            &mut map,
            &op(
                7,
                1,
                &[
                    event(OTHER, dispatched("duck", 3)),
                    event("chat\x1fgeneral\x1f3\x1fbot", dispatched("bot", 3)),
                ],
            ),
        );
        let ids: Vec<String> = recent(&map, None)
            .into_iter()
            .map(|run| run.run_id)
            .collect();
        assert_eq!(ids, ["chat\x1fgeneral\x1f3\x1fbot", OTHER, RUN]);
        let bots: Vec<String> = recent(&map, Some("bot"))
            .into_iter()
            .map(|run| run.run_id)
            .collect();
        assert_eq!(bots, ["chat\x1fgeneral\x1f3\x1fbot", RUN]);
        assert!(recent(&map, Some("nobody")).is_empty());
    }

    #[test]
    fn a_settled_run_takes_its_pr_link_and_a_result_action_refusal() {
        let mut map = Map::new();
        fold(&mut map, &op(1, 0, &[event(RUN, dispatched("bot", 2))]));
        fold(
            &mut map,
            &op(2, 0, &[event(RUN, settled(RunOutcome::ResultAccepted))]),
        );
        let pr = PrRef {
            repo: "playground".into(),
            number: 12,
        };
        fold(
            &mut map,
            &op(3, 0, &[event(RUN, RunFact::PrLinked { pr: pr.clone() })]),
        );
        let run = &recent(&map, None)[0];
        assert_eq!(run.places, [pr_place(&pr)]);
        fold(
            &mut map,
            &op(
                4,
                0,
                &[event(
                    RUN,
                    RunFact::ResultActionRefused {
                        request_id: "result/x/0".into(),
                    },
                )],
            ),
        );
        let run = &recent(&map, None)[0];
        assert!(
            matches!(
                &run.state,
                RunState::Settled {
                    outcome: RunOutcome::ActionRejected,
                    ..
                }
            ),
            "{:?}",
            run.state
        );
        // a failure is never upgraded by a refusal
        fold(&mut map, &op(5, 0, &[event(OTHER, dispatched("duck", 3))]));
        fold(
            &mut map,
            &op(6, 0, &[event(OTHER, settled(RunOutcome::Failed))]),
        );
        fold(
            &mut map,
            &op(
                7,
                0,
                &[event(
                    OTHER,
                    RunFact::ResultActionRefused {
                        request_id: "result/y/0".into(),
                    },
                )],
            ),
        );
        let other = detail(&map, OTHER).unwrap().run;
        assert!(matches!(
            other.state,
            RunState::Settled {
                outcome: RunOutcome::Failed,
                ..
            }
        ));
    }

    /// THE PLACES ARE THE JOURNAL'S RECEIPTS, each named once: the origin the
    /// run answers, every destination a receipt resolved or id it minted, the
    /// output and the PR it settled with — and a PR the settle found is the
    /// same place the link later confirms.
    #[test]
    fn a_run_names_its_origin_and_every_place_its_receipts_touched_once() {
        let mut map = Map::new();
        fold(&mut map, &op(1, 0, &[event(RUN, dispatched("bot", 2))]));
        let run = &recent(&map, None)[0];
        assert_eq!(
            run.origin,
            Some(RunPlace::ChatMessage {
                channel_id: "general".into(),
                seq: 2
            })
        );
        assert!(run.places.is_empty());

        let callee = delegated_run_id_for("d1", "helper");
        fold(
            &mut map,
            &op(
                2,
                0,
                &[
                    event(
                        RUN,
                        acted(
                            "r0",
                            OP_REPLY,
                            serde_json::json!({
                                "destination": {"kind": "chat", "channel_id": "general", "thread": 2},
                                "id": "agent/x",
                            }),
                        ),
                    ),
                    event(
                        RUN,
                        acted(
                            "r1",
                            OP_CHAT_POST_MESSAGE,
                            serde_json::json!({"channel_id": "general", "thread": 2, "message_id": "agent/x/post/s1"}),
                        ),
                    ),
                    event(
                        RUN,
                        acted(
                            "r2",
                            OP_PAGES_POST,
                            serde_json::json!({"page_id": "p9", "title": "Duck poem"}),
                        ),
                    ),
                    event(
                        RUN,
                        acted(
                            "r3",
                            OP_PAGES_COMMENT,
                            serde_json::json!({"target": "b4", "thread_id": "t4", "comment_id": "c1"}),
                        ),
                    ),
                    event(
                        RUN,
                        acted("r4", OP_TASKS_CREATE, serde_json::json!({"task_id": "t-1"})),
                    ),
                    event(
                        RUN,
                        acted(
                            "r5",
                            OP_DUCKFS_WRITE_TEXT,
                            serde_json::json!({"path": "/shared/poem.md", "base_snapshot": null}),
                        ),
                    ),
                    event(
                        RUN,
                        acted(
                            "r6",
                            OP_AGENT_CALL,
                            serde_json::json!({"delegation_id": "d1", "callee_agent_id": "helper"}),
                        ),
                    ),
                    // a reaction names the anchor the origin already does
                    event(
                        RUN,
                        acted("r7", crate::OP_REACT, serde_json::json!({"emoji": "👀"})),
                    ),
                    // the forge sink's label carries no receipt
                    event(RUN, acted("r8", "forge", serde_json::Value::Null)),
                ],
            ),
        );
        let pr = PrRef {
            repo: "playground".into(),
            number: 7,
        };
        fold(
            &mut map,
            &op(
                3,
                0,
                &[event(
                    RUN,
                    RunFact::Settled {
                        outcome: RunOutcome::ResultAccepted,
                        reason: None,
                        degraded: false,
                        executing_node: "ab".into(),
                        output_ref: Some("agent/poem@abc123".into()),
                        pr: Some(pr.clone()),
                    },
                )],
            ),
        );
        fold(
            &mut map,
            &op(4, 0, &[event(RUN, RunFact::PrLinked { pr: pr.clone() })]),
        );
        let run = detail(&map, RUN).unwrap().run;
        assert_eq!(run.actions, 9);
        assert_eq!(
            run.places,
            [
                RunPlace::ChatMessage {
                    channel_id: "general".into(),
                    seq: 2
                },
                RunPlace::Page {
                    page_id: "p9".into(),
                    title: "Duck poem".into()
                },
                RunPlace::PageBlock {
                    block_id: "b4".into()
                },
                RunPlace::Task {
                    task_id: "t-1".into()
                },
                RunPlace::File {
                    path: "/shared/poem.md".into()
                },
                RunPlace::Run {
                    dispatch_id: dispatch_id_for(&callee)
                },
                RunPlace::Output {
                    output_ref: "agent/poem@abc123".into()
                },
                pr_place(&pr),
            ]
        );
    }

    #[test]
    fn a_run_called_from_a_page_a_job_or_a_forge_item_names_that_origin() {
        let origin = |channel_id: &str, anchor_seq: u64, job_id: Option<&str>| {
            origin_place(channel_id, anchor_seq, &job_id.map(str::to_string))
        };
        assert_eq!(
            origin("", 0, Some("job-1")),
            Some(RunPlace::Job {
                job_id: "job-1".into()
            })
        );
        assert_eq!(
            origin(&crate::page_channel_id("t7"), 1, None),
            Some(RunPlace::PageThread {
                thread_id: "t7".into()
            })
        );
        assert_eq!(
            origin(&crate::page_block_channel_id("b3"), 1, None),
            Some(RunPlace::PageBlock {
                block_id: "b3".into()
            })
        );
        assert_eq!(
            origin("forge:playground:4", 1, None),
            Some(RunPlace::ForgeItem {
                repo: "playground".into(),
                number: 4
            })
        );
        // a delegated run answers a caller run, not a place
        assert_eq!(origin("", 0, None), None);
    }

    #[test]
    fn an_op_that_moved_no_run_and_a_fact_about_an_unseen_run_write_only_what_they_can() {
        let mut map = Map::new();
        assert!(fold_op(&op(1, 0, &[]), &map).unwrap().is_empty());
        // installed after the dispatch: the journal row lands, no run row
        fold(
            &mut map,
            &op(2, 0, &[event(RUN, settled(RunOutcome::Failed))]),
        );
        assert!(recent(&map, None).is_empty());
        assert_eq!(detail(&map, RUN), None);
        assert_eq!(journal_of(&map, &dispatch_id_for(RUN)).unwrap().len(), 1);
    }
}
