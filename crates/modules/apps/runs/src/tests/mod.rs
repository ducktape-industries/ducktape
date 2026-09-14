use super::*;
use crate::facets::{WireSink, decode_run_result};
use crate::response::{
    FAILURE_EXCERPT_BYTES, agent_response_from_text, failure_excerpt, parse_strict_response,
};
use crate::{OP_CHAT_POST_MESSAGE, OP_TASKS_CREATE, OP_TASKS_UPDATE_STATUS};
use crate::{decode_reply as runs_decode_reply, encode_msg, encode_query};
use base64::Engine as _;
use chat::{Channel, MessageHead, Party, decode_msg as chat_decode_msg};
use dispatch::{
    DispatchStatus, DispatchView, decode_msg as dispatch_decode_msg,
    encode_reply as dispatch_encode_reply,
};
use files::{
    decode_msg as files_decode_msg, decode_query as files_decode_query,
    encode_reply as files_encode_reply,
};
use futures::executor::block_on;
use sdk::Env;
use std::cell::{Cell, RefCell};
use tasks::{
    Claim as JobClaim, Job, Task, decode_task_msg as tasks_decode_msg,
    encode_job_event as jobs_encode_event, encode_job_reply as jobs_encode_reply,
    encode_task_reply as tasks_encode_reply,
};

/// Model configurations keyed by agent id, installed directly into the Runs
/// fixture before executing an operation.
type Registry = BTreeMap<String, ModelRecord>;

/// a minimal `Ctx` that captures emitted msgs/effects/events and serves
/// model fixture records, chat transcripts, task lists, job records,
/// and dispatch records — enough to unit-test `execute` in isolation
/// (the host provides the real routing in integration).
struct CaptureCtx {
    env: Env,
    query_count: Cell<usize>,
    query_keys: RefCell<BTreeSet<(String, Vec<u8>)>>,
    /// Model records copied into the Runs fixture by `exec`.
    agents: Registry,
    /// channel -> messages with contiguous seqs starting at 1.
    transcripts: BTreeMap<String, Vec<MessageView>>,
    /// channels the "chat" Access arm treats as members-only — every other
    /// channel with a transcript answers open standing to any user, mirroring
    /// chat's own `PostPolicy::Open` default in the "Channel" arm above.
    members_only: BTreeSet<String>,
    /// channel -> the users a members-only channel admits, served by the
    /// "chat" Access arm (`ChatQuery::Access`/`may_post`/`may_read`).
    members: BTreeMap<String, BTreeSet<Vec<u8>>>,
    tasks: Vec<Task>,
    /// dispatch ids the dispatch module already has a record for — the
    /// committed turn-claim layer the module probes.
    taken_dispatches: BTreeSet<String>,
    /// dispatch ids that are still AwaitingResult (their saga carries the live
    /// lease the session lane authorizes against). one only in `taken_dispatches`
    /// is delivered, and a delivered run runs nowhere. the value is the lease
    /// holder, mirrored into `sagas` by `with_lease_holder`.
    dispatch_assignees: BTreeMap<String, Vec<u8>>,
    /// job_id -> board record served by the jobs arm (finalize guard).
    jobs: BTreeMap<String, Job>,
    /// repo -> born (branch, tip-hex) pairs, served by the "forge"
    /// ListRefs arm (the sink's branch-born probe and the compose lane's
    /// commit pinning).
    forge_refs: BTreeMap<String, Vec<(String, String)>>,
    /// (repo, number) -> tracker item, served by the "forge" GetItem arm
    /// (the compose lane's committed item lookup) and, as summaries, by the
    /// ListItems arm (the sink's duplicate-PR guard).
    forge_items: BTreeMap<(String, u64), forge::ItemDetail>,
    /// Saga views distinguish pending execution leases from terminal results.
    sagas: BTreeMap<String, saga::SagaView>,
    /// page_id -> the canonical whole page in preorder, sliced by the "pages"
    /// GetPage arm (the `duck://page/<id>` injection lane); GetBlock scans the
    /// same pages by block id (the pages-effects target resolution).
    pages: BTreeMap<String, Vec<pages::Block>>,
    page_query_count: Cell<usize>,
    page_query_fail_after: Option<usize>,
    /// explicit committed page comment threads used by Pages-triggered runs.
    page_threads: BTreeMap<String, pages::ThreadView>,
    /// thread/comment ids the pages module already holds — the squat
    /// simulation the CommentThread/GetComment freshness probes hit.
    taken_page_ids: BTreeSet<String>,
    /// target -> committed thread count served by the TargetThreadCount arm
    /// (the capacity probe); absent targets serve zero threads.
    page_target_threads: BTreeMap<String, usize>,
    /// the committed duckfs head served by the "files" Refs arm — the v1
    /// composer's `source_snapshot` pin. `None` = a fresh network (null pin).
    files_head: Option<String>,
    /// committed attachment bytes served by the "files" Read arm, keyed by
    /// absolute path (the duck://files injection resolves these).
    files_content: BTreeMap<String, Vec<u8>>,
    msgs: Vec<Msg>,
    #[allow(dead_code)]
    events: Vec<Event>,
    /// the op's assigned stamp: the run journal the module committed.
    assigned: Vec<u8>,
}
impl CaptureCtx {
    fn new() -> Self {
        Self {
            env: Env {
                height: 0,
                consensus_time: 0,
                origin: Origin::System,
                me: "runs".into(),
                cause: sdk::Cause::Direct,
            },
            query_count: Cell::new(0),
            query_keys: RefCell::new(BTreeSet::new()),
            agents: Registry::new(),
            transcripts: BTreeMap::new(),
            members_only: BTreeSet::new(),
            members: BTreeMap::new(),
            tasks: Vec::new(),
            taken_dispatches: BTreeSet::new(),
            dispatch_assignees: BTreeMap::new(),
            jobs: BTreeMap::new(),
            forge_refs: BTreeMap::new(),
            forge_items: BTreeMap::new(),
            sagas: BTreeMap::new(),
            pages: BTreeMap::new(),
            page_query_count: Cell::new(0),
            page_query_fail_after: None,
            page_threads: BTreeMap::new(),
            taken_page_ids: BTreeSet::new(),
            page_target_threads: BTreeMap::new(),
            files_head: None,
            files_content: BTreeMap::new(),
            msgs: Vec::new(),
            events: Vec::new(),
            assigned: Vec::new(),
        }
    }

    /// the run journal the last op stamped; empty when it moved no run.
    fn journal(&self) -> Vec<RunEvent> {
        if self.assigned.is_empty() {
            return Vec::new();
        }
        decode_assigned(&self.assigned).expect("a runs stamp decodes")
    }
    fn at(mut self, view: u64) -> Self {
        self.env.height = view;
        self.env.consensus_time = view;
        self
    }
    fn query_count(&self) -> usize {
        self.query_count.get()
    }
    fn distinct_query_count(&self) -> usize {
        self.query_keys.borrow().len()
    }
    /// register a born branch under `repo` (the sink's branch-born probe).
    /// tip = the fixed oid facets.rs's forge-sink fixtures use as their
    /// receipt `output_commit` (`"1a".repeat(20)`) — #1835 requires
    /// `emit_sink` to see the receipt's oid AS the branch's committed tip,
    /// so a caller after a fixed zero tip that never needs to match a
    /// receipt wants [`Self::with_forge_tip`] instead.
    fn with_forge_ref(self, repo: &str, branch: &str) -> Self {
        let tip = "1a".repeat(20);
        self.with_forge_tip(repo, branch, &tip)
    }
    /// register a born branch with an explicit tip (the compose lane's
    /// commit pinning).
    fn with_forge_tip(mut self, repo: &str, branch: &str, tip: &str) -> Self {
        self.forge_refs
            .entry(repo.into())
            .or_default()
            .push((branch.into(), tip.into()));
        self
    }
    /// register a tracker item served by the "forge" GetItem arm.
    fn with_forge_item(mut self, repo: &str, item: forge::ItemDetail) -> Self {
        self.forge_items
            .insert((repo.into(), item.summary.number), item);
        self
    }
    /// register the node key holding `saga_id`'s winning lease, served by
    /// the "saga" Get arm (the sink's executing-node attribution).
    fn with_saga_assignee(mut self, saga_id: &str, key: &[u8]) -> Self {
        self.sagas
            .insert(saga_id.into(), saga_view(key, 0, saga::SagaStatus::Done));
        self
    }
    /// register a committed page (whole preorder Vec, root first) served in
    /// bounded slices by the "pages" GetPage arm.
    fn with_page(mut self, page_id: &str, blocks: Vec<pages::Block>) -> Self {
        self.pages.insert(page_id.into(), blocks);
        self
    }
    fn fail_page_queries_after(mut self, successful_queries: usize) -> Self {
        self.page_query_fail_after = Some(successful_queries);
        self
    }
    fn page_query_count(&self) -> usize {
        self.page_query_count.get()
    }
    fn with_page_thread(mut self, view: pages::ThreadView) -> Self {
        self.page_threads.insert(view.thread.id.clone(), view);
        self
    }
    /// mark a thread/comment id as already minted in the pages module (the
    /// freshness probes then see it taken).
    fn with_taken_page_id(mut self, id: &str) -> Self {
        self.taken_page_ids.insert(id.into());
        self
    }
    /// mark a target as already holding the thread cap.
    fn with_crowded_page_target(self, target: &str) -> Self {
        self.with_page_target_threads(target, pages::MAX_THREADS_PER_TARGET)
    }
    /// serve `count` committed threads for `target` (the capacity probe).
    fn with_page_target_threads(mut self, target: &str, count: usize) -> Self {
        self.page_target_threads.insert(target.into(), count);
        self
    }
    /// set the committed duckfs head the "files" Refs arm serves (the v1
    /// composer's `source_snapshot`).
    fn with_files_head(mut self, head: &str) -> Self {
        self.files_head = Some(head.into());
        self
    }
    /// serve committed bytes at `path` from the "files" Read arm — the
    /// duck://files attachment injection reads these.
    fn with_file(mut self, path: &str, bytes: &[u8]) -> Self {
        self.files_content.insert(path.into(), bytes.to_vec());
        self
    }
    fn with_origin(mut self, origin: Origin) -> Self {
        self.env.origin = origin;
        self
    }
    fn with_program_origin(self) -> Self {
        self.with_origin(Origin::Program(2))
    }
    fn with_dispatch_origin(self) -> Self {
        self.with_origin(Origin::Module("dispatch".into()))
    }
    fn with_jobs_origin(self) -> Self {
        self.with_origin(Origin::Module("jobs".into()))
    }
    fn with_agent_origin(self) -> Self {
        self.with_origin(Origin::Module("agent".into()))
    }
    fn with_registry(mut self, registry: &Registry) -> Self {
        self.agents = registry.clone();
        self
    }
    fn with_transcript(mut self, channel: &str, messages: Vec<MessageView>) -> Self {
        self.transcripts.insert(channel.into(), messages);
        self
    }
    /// mark `channel` members-only and admit `member` — the "chat" Access
    /// arm then answers `may_post`/`may_read` for that user alone.
    fn with_members_only(mut self, channel: &str, member: Vec<u8>) -> Self {
        self.members_only.insert(channel.into());
        self.members
            .entry(channel.into())
            .or_default()
            .insert(member);
        self
    }
    fn with_task(mut self, id: &str) -> Self {
        self.tasks.push(Task {
            id: id.into(),
            title: id.into(),
            status: TaskStatus::Open,
            owner: tasks::Party::Module("test".into()),
            created_at: 0,
            updated_at: 0,
        });
        self
    }
    fn with_taken_dispatch(mut self, dispatch_id: &str) -> Self {
        self.taken_dispatches.insert(dispatch_id.into());
        self
    }
    /// serve `key` as the node holding `run_id`'s execution lease — an awaiting
    /// dispatch (the "saga" Get resolves its committed lease to `key`), the ONLY
    /// origin the session lane lets open a session.
    fn with_lease_holder(self, run_id: &str, key: &[u8]) -> Self {
        self.with_lease_attempt(run_id, key, 0)
    }

    fn with_lease_attempt(mut self, run_id: &str, key: &[u8], attempt: u32) -> Self {
        let dispatch_id = dispatch_id_for(run_id);
        self.sagas.insert(
            crate::sink::saga_id_for_dispatch("runs", &dispatch_id),
            saga_view(key, attempt, saga::SagaStatus::Pending),
        );
        self.dispatch_assignees.insert(dispatch_id, key.to_vec());
        self
    }
    /// serve `run_id` as an awaiting dispatch whose saga holds NO committed
    /// lease (the "saga" Get answers `None`) — the missing-lease refusal path.
    fn with_awaiting_but_no_lease(mut self, run_id: &str) -> Self {
        self.dispatch_assignees
            .insert(dispatch_id_for(run_id), Vec::new());
        self
    }
    /// a job the board holds as Processing, claimed by "runs" at `height`.
    fn with_claimed_job(mut self, job_id: &str, height: u64) -> Self {
        self.jobs.insert(
            job_id.into(),
            Job {
                job_id: job_id.into(),
                kind: "agent/duck".into(),
                spec: "spec".into(),
                submitter: tasks::Party::Key(vec![1]),
                status: JobStatus::Processing,
                attempt: 1,
                claim: Some(JobClaim {
                    worker: tasks::Party::Module("runs".into()),
                    claimed_at_height: height,
                    lease_views: JOB_RUN_LEASE_VIEWS,
                }),
                result: None,
                comments: Vec::new(),
                created_at_revision: 1,
                created_at_height: height,
                updated_at_height: height,
            },
        );
        self
    }
    /// decoded chat msgs emitted this dispatch.
    fn chat_msgs(&self) -> Vec<ChatMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "chat")
            .map(|m| chat_decode_msg(&m.payload).expect("chat msg"))
            .collect()
    }
    /// decoded task msgs emitted this dispatch.
    fn task_msgs(&self) -> Vec<TaskMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "tasks")
            .map(|m| tasks_decode_msg(&m.payload).expect("task msg"))
            .collect()
    }
    /// decoded jobs msgs emitted this dispatch.
    fn job_msgs(&self) -> Vec<JobsMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "jobs")
            .map(|m| tasks::decode_job_msg(&m.payload).expect("jobs msg"))
            .collect()
    }
    /// decoded dispatch-plane msgs emitted this dispatch.
    fn dispatch_msgs(&self) -> Vec<DispatchMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "dispatch")
            .map(|m| dispatch_decode_msg(&m.payload).expect("dispatch msg"))
            .collect()
    }
    /// decoded pages msgs emitted this dispatch.
    fn page_msgs(&self) -> Vec<pages::PageMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "pages")
            .map(|m| pages::decode_msg(&m.payload).expect("pages msg"))
            .collect()
    }
    /// decoded collaboration msgs emitted this dispatch, each with the network
    /// its request is bound to — the binding is half the payload's meaning.
    fn collaboration_msgs(&self) -> Vec<collaboration::Request> {
        self.msgs
            .iter()
            .filter(|m| m.target == "collaboration")
            .map(|m| collaboration::decode_msg(&m.payload).expect("collaboration msg"))
            .collect()
    }
    /// decoded files msgs emitted this dispatch.
    fn files_msgs(&self) -> Vec<FilesMsg> {
        self.msgs
            .iter()
            .filter(|m| m.target == "files")
            .map(|m| files_decode_msg(&m.payload).expect("files msg"))
            .collect()
    }
    /// the breadcrumb notes emitted this dispatch, as strings.
    fn notes(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
            .collect()
    }
}

/// a minimal committed thread view, as the "pages" CommentThread arm serves a
/// taken thread id.
fn dummy_thread_view(id: &str) -> pages::ThreadView {
    pages::ThreadView {
        thread: pages::Thread {
            id: id.into(),
            target: "elsewhere".into(),
            opener: pages::Party::System,
            created_at: 0,
            anchor: None,
            resolved: false,
            resolved_by: None,
            comment_ids: Vec::new(),
        },
        comments: Vec::new(),
    }
}

/// a minimal committed comment, as the "pages" GetComment arm serves a taken
/// comment id.
fn dummy_comment(id: &str) -> pages::Comment {
    pages::Comment {
        id: id.into(),
        thread_id: "elsewhere".into(),
        author: pages::Party::System,
        mentions: Vec::new(),
        text: String::new(),
        created_at: 0,
        edited_at: None,
        deleted: false,
    }
}
#[async_trait::async_trait(?Send)]
impl Ctx for CaptureCtx {
    fn env(&self) -> &Env {
        &self.env
    }
    fn module_root(&self, _target: &str) -> Option<StateRoot> {
        None
    }
    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        let query_count = self.query_count.get();
        self.query_count.set(query_count + 1);
        self.query_keys
            .borrow_mut()
            .insert((target.to_string(), req.to_vec()));
        match target {
            "identity" => {
                let query: identity::IdentityQuery =
                    identity::decode_query(req).map_err(Error::Module)?;
                let number = match query {
                    identity::IdentityQuery::Get { number } => number,
                    identity::IdentityQuery::OfKey { .. } => 1,
                    _ => return Err(Error::QueryUnsupported),
                };
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    Some(identity::AccountView {
                        number,
                        name: "fixture".into(),
                        control: identity::Control::Program {
                            controller: 1,
                            executor: "agent".into(),
                            generation: 0,
                            standing: identity::ProgramStanding::Active,
                        },
                        keys: Vec::new(),
                        avatar: None,
                        bio: None,
                        updated_at: 0,
                    }),
                )))
            }
            "chat" => match chat::decode_query(req).map_err(Error::Module)? {
                ChatQuery::MessagesRange {
                    channel_id,
                    from_seq,
                    limit,
                } => {
                    let transcript = self
                        .transcripts
                        .get(&channel_id)
                        .ok_or_else(|| Error::Module(format!("unknown channel: {channel_id}")))?;
                    let head = transcript.len() as u64;
                    let from = from_seq.max(1);
                    let mut window = Vec::new();
                    if limit > 0 && from <= head {
                        let to = head.min(from + limit - 1);
                        window = transcript[(from - 1) as usize..to as usize].to_vec();
                    }
                    Ok(chat::encode_reply(&ChatReply::Messages(window)))
                }
                ChatQuery::Message { message_id } => Ok(chat::encode_reply(&ChatReply::Message(
                    self.transcripts
                        .values()
                        .flatten()
                        .find(|v| v.head.message_id == message_id)
                        .cloned(),
                ))),
                // a channel EXISTS exactly when it has a transcript — the
                // fixture idiom already in use everywhere (a channel with no
                // messages is `with_transcript(ch, vec![])`).
                ChatQuery::Channel { channel_id } => Ok(chat::encode_reply(&ChatReply::Channel(
                    self.transcripts.get(&channel_id).map(|msgs| Channel {
                        id: channel_id.clone(),
                        name: channel_id,
                        created_at: 0,
                        head_seq: msgs.len() as u64,
                        post_policy: chat::PostPolicy::Open,
                        hooks: Vec::new(),
                        pinned: Vec::new(),
                        huddle: Vec::new(),
                        voice: false,
                        owner: chat::Party::System,
                        revision: 1,
                        archived: false,
                    }),
                ))),
                // an explicit RequestRun asks this before pinning a
                // members-only channel's transcript; every other channel with
                // a transcript answers open standing, mirroring the "Channel"
                // arm's hardcoded `PostPolicy::Open`.
                ChatQuery::Access { channel_id, party } => {
                    let user = match party {
                        chat::Party::Key(key) => key,
                        chat::Party::Account(_) => vec![9; 32],
                        _ => Vec::new(),
                    };
                    let may_post = if self.members_only.contains(&channel_id) {
                        self.members
                            .get(&channel_id)
                            .is_some_and(|m| m.contains(&user))
                    } else {
                        true
                    };
                    Ok(chat::encode_reply(&ChatReply::Access(
                        chat::ChannelAccess {
                            may_read: may_post,
                            may_post,
                        },
                    )))
                }
            },
            // the board answers the SAME two reads the real module does: the
            // by-id `Get` the validator probes with, and a bounded `List` page.
            "tasks" => match tasks::decode_task_query(req).map_err(Error::Module)? {
                TaskQuery::Get { task_id } => Ok(tasks_encode_reply(&TaskReply::Task(
                    self.tasks.iter().find(|t| t.id == task_id).cloned(),
                ))),
                TaskQuery::List { limit, after } => {
                    let page = self
                        .tasks
                        .iter()
                        .filter(|t| after.as_deref().is_none_or(|cursor| t.id.as_str() > cursor))
                        .take(limit.clamp(1, tasks::MAX_LIST_LIMIT) as usize)
                        .cloned()
                        .collect();
                    Ok(tasks_encode_reply(&TaskReply::Tasks(page)))
                }
                TaskQuery::OwnerOpenCount { owner } => {
                    let count = self.tasks.iter().filter(|t| t.owner == owner).count() as u64;
                    Ok(tasks_encode_reply(&TaskReply::OwnerOpenCount(count)))
                }
            },
            "jobs" => match tasks::decode_job_query(req).map_err(Error::Module)? {
                JobsQuery::Get { job_id } => Ok(jobs_encode_reply(&JobsReply::Job(
                    self.jobs.get(&job_id).cloned(),
                ))),
            },
            "dispatch" => match dispatch::decode_query(req).map_err(Error::Module)? {
                DispatchQuery::Dispatch { dispatch_id, .. } => {
                    // an awaiting dispatch still names its saga (the lease lives
                    // there); a merely `taken` one already delivered.
                    let awaiting = self.dispatch_assignees.contains_key(&dispatch_id);
                    let view =
                        (awaiting || self.taken_dispatches.contains(&dispatch_id)).then(|| {
                            DispatchView {
                                cause: sdk::Cause::Direct,
                                recipe_id: "agent/x".into(),
                                receiver: "runs".into(),
                                status: if awaiting {
                                    DispatchStatus::AwaitingResult {
                                        saga_id: crate::sink::saga_id_for_dispatch(
                                            "runs",
                                            &dispatch_id,
                                        ),
                                    }
                                } else {
                                    DispatchStatus::Delivered {
                                        delivery: sdk::DeliveryOutcome::Applied,
                                    }
                                },
                                outcome: (!awaiting).then(|| Ok(Vec::new())),
                                dispatch_id,
                                created_at: 0,
                                updated_at: 0,
                            }
                        });
                    Ok(dispatch_encode_reply(&DispatchReply::Dispatch(view)))
                }
                _ => Err(Error::QueryUnsupported),
            },
            "files" => match files_decode_query(req).map_err(Error::Module)? {
                FilesQuery::Refs {} => Ok(files_encode_reply(&FilesReply::Refs(files::RefsInfo {
                    head: self.files_head.clone(),
                    pins: BTreeMap::new(),
                    window_len: 0,
                }))),
                FilesQuery::Read {
                    path, offset, len, ..
                } => {
                    let reply = match self.files_content.get(&path) {
                        Some(bytes) => {
                            let start = (offset as usize).min(bytes.len());
                            let end = (start + len as usize).min(bytes.len());
                            let slice = &bytes[start..end];
                            FilesReply::Read {
                                b64: base64::engine::general_purpose::STANDARD.encode(slice),
                                eof: end == bytes.len(),
                            }
                        }
                        // an absent path answers as a module rejection (a 404
                        // on the real node) — the injector reads that as absent.
                        None => return Err(Error::QueryUnsupported),
                    };
                    Ok(files_encode_reply(&reply))
                }
                // the per-path CAS probe (`response::probe_duckfs_write_base`)
                // reads this. the mock has no real snapshot history, so it
                // serves the SAME committed entry regardless of `snapshot` —
                // enough to model "path exists" vs "path is new", which is all
                // a duckfs write test needs.
                FilesQuery::Stat { path, .. } => {
                    let entry = self.files_content.get(&path).map(|bytes| files::EntryInfo {
                        path: path.clone(),
                        kind: files::EntryKindWire::File,
                        size: bytes.len() as u64,
                        exec: false,
                        object: "00".repeat(32),
                        meta: BTreeMap::new(),
                    });
                    Ok(files_encode_reply(&FilesReply::Stat(entry)))
                }
                _ => Err(Error::QueryUnsupported),
            },
            "forge" => match forge::decode_query(req).map_err(Error::Module)? {
                forge::ForgeQuery::ListRefs { repo } => {
                    let refs = self
                        .forge_refs
                        .get(&repo)
                        .into_iter()
                        .flatten()
                        .map(|(name, tip)| forge::RefHead {
                            name: name.clone(),
                            head: tip.clone(),
                        })
                        .collect();
                    Ok(forge::encode_reply(&forge::ForgeReply::Refs(refs)))
                }
                forge::ForgeQuery::GetItem { repo, number } => {
                    Ok(forge::encode_reply(&forge::ForgeReply::Item(
                        self.forge_items.get(&(repo, number)).cloned().map(Box::new),
                    )))
                }
                forge::ForgeQuery::ListItems { repo } => {
                    // ascending by number — the BTreeMap key order, exactly
                    // like the real tracker's listing.
                    let items = self
                        .forge_items
                        .iter()
                        .filter(|((r, _), _)| *r == repo)
                        .map(|(_, d)| d.summary.clone())
                        .collect();
                    Ok(forge::encode_reply(&forge::ForgeReply::Items(items)))
                }
                _ => Err(Error::QueryUnsupported),
            },
            "pages" => match pages::decode_query(req).map_err(Error::Module)? {
                pages::PageQuery::GetPage {
                    page_id,
                    after,
                    limit,
                } => {
                    let query_count = self.page_query_count.get();
                    self.page_query_count.set(query_count + 1);
                    if self.page_query_fail_after == Some(query_count) {
                        return Err(Error::QueryUnsupported);
                    }
                    let Some(blocks) = self.pages.get(&page_id) else {
                        return Ok(pages::encode_reply(&pages::PageReply::Page(None)));
                    };
                    let start = match after {
                        Some(cursor) => blocks
                            .iter()
                            .position(|block| block.id == cursor)
                            .and_then(|index| index.checked_add(1))
                            .ok_or(Error::QueryUnsupported)?,
                        None => 0,
                    };
                    let limit = if limit == 0 {
                        pages::MAX_PAGE_QUERY_LIMIT
                    } else {
                        limit.min(pages::MAX_PAGE_QUERY_LIMIT)
                    };
                    let end = start.saturating_add(usize::from(limit)).min(blocks.len());
                    let page_blocks = blocks[start..end].to_vec();
                    let has_more = end < blocks.len();
                    let next_after = has_more.then(|| {
                        page_blocks
                            .last()
                            .expect("a non-final page contains a block")
                            .id
                            .clone()
                    });
                    Ok(pages::encode_reply(&pages::PageReply::Page(Some(
                        pages::PageBlockPage {
                            blocks: page_blocks,
                            next_after,
                        },
                    ))))
                }
                pages::PageQuery::GetBlock { block_id } => {
                    Ok(pages::encode_reply(&pages::PageReply::Block(
                        self.pages
                            .values()
                            .flatten()
                            .find(|b| b.id == block_id)
                            .cloned(),
                    )))
                }
                pages::PageQuery::CommentThreadHead { thread_id } => {
                    let view = self.page_threads.get(&thread_id).cloned().or_else(|| {
                        self.taken_page_ids
                            .contains(&thread_id)
                            .then(|| dummy_thread_view(&thread_id))
                    });
                    Ok(pages::encode_reply(&pages::PageReply::CommentThreadHead(
                        view.map(|view| pages::CommentThreadHead {
                            target: view.thread.target,
                            comment_count: view.thread.comment_ids.len() as u64,
                        }),
                    )))
                }
                pages::PageQuery::CommentThread { thread_id } => {
                    Ok(pages::encode_reply(&pages::PageReply::CommentThread(
                        self.page_threads.get(&thread_id).cloned().or_else(|| {
                            self.taken_page_ids
                                .contains(&thread_id)
                                .then(|| dummy_thread_view(&thread_id))
                        }),
                    )))
                }
                pages::PageQuery::GetComment { comment_id } => {
                    Ok(pages::encode_reply(&pages::PageReply::Comment(
                        self.taken_page_ids
                            .contains(&comment_id)
                            .then(|| dummy_comment(&comment_id)),
                    )))
                }
                pages::PageQuery::TargetThreadCount { target } => {
                    let count = self.page_target_threads.get(&target).copied().unwrap_or(0);
                    Ok(pages::encode_reply(&pages::PageReply::TargetThreadCount(
                        count as u64,
                    )))
                }
                pages::PageQuery::PageCount => Ok(pages::encode_reply(
                    &pages::PageReply::PageCount(self.pages.len() as u64),
                )),
            },
            "saga" => match saga::decode_query(req).map_err(Error::Module)? {
                saga::SagaQuery::Get { saga_id } => {
                    let view = self.sagas.get(&saga_id).cloned();
                    Ok(saga::encode_reply(&saga::SagaReply::Saga(view)))
                }
                _ => Err(Error::QueryUnsupported),
            },
            other => Err(Error::UnknownModule(other.into())),
        }
    }
    fn emit_msg(&mut self, msg: Msg) {
        self.msgs.push(msg);
    }
    fn emit_event(&mut self, ev: Event) {
        self.events.push(ev);
    }
    fn set_assigned(&mut self, bytes: Vec<u8>) {
        self.assigned = bytes;
    }
}

// ---- fixtures -----------------------------------------------------------

fn module() -> RunsModule {
    RunsModule::new(
        "runs",
        "chat",
        "saga",
        "attribution",
        "dispatch",
        "agent",
        Some("tasks".into()),
        Some("jobs".into()),
    )
}

fn user(byte: u8) -> Origin {
    Origin::External(vec![byte; 32])
}

fn record(agent_id: &str) -> ModelRecord {
    ModelRecord {
        account: 2,
        agent_id: agent_id.into(),
        owner: RunOrigin::External(vec![9; 32]),
        display_name: agent_id.to_uppercase(),
        capability: "model-1".into(),
        status: ModelStatus::Active,
        role: crate::ModelRole::General,
        created_at: 0,
        updated_at: 0,
        recipe_hash: Vec::new(),
        skills: Vec::new(),
    }
}

fn registry(agents: &[&str]) -> Registry {
    agents
        .iter()
        .map(|id| ((*id).to_string(), record(id)))
        .collect()
}

fn pause(registry: &mut Registry, agent_id: &str) {
    registry.get_mut(agent_id).expect("registered").status = ModelStatus::Paused;
}

fn message_in(
    channel: &str,
    seq: u64,
    author: Party,
    text: &str,
    thread: Option<u64>,
) -> MessageView {
    MessageView {
        channel_id: channel.into(),
        seq,
        head: MessageHead {
            message_id: format!("{channel}-m{seq}"),
            content_origin: match &author {
                chat::Party::Key(key) => sdk::Origin::External(key.clone()),
                chat::Party::Account(account) => sdk::Origin::Program(*account),
                chat::Party::Module(module) => sdk::Origin::Module(module.clone()),
                chat::Party::System => sdk::Origin::System,
            },
            origin: match &author {
                chat::Party::Key(key) => sdk::Origin::External(key.clone()),
                chat::Party::Account(account) => sdk::Origin::Program(*account),
                chat::Party::Module(module) => sdk::Origin::Module(module.clone()),
                chat::Party::System => sdk::Origin::System,
            },
            author,
            blocks: vec![Block::paragraph(text)],
            created_at: 0,
            rev: 0,
            revision: 1,
            edited_at: None,
            base_rev: None,
            deleted: false,
            thread,
            reply_count: 0,
            last_reply_seq: None,
        },
    }
}

fn message(seq: u64, text: &str) -> MessageView {
    message_in("general", seq, Party::Key(vec![1; 32]), text, None)
}

fn transcript(n: u64) -> Vec<MessageView> {
    (1..=n).map(|i| message(i, &format!("msg {i}"))).collect()
}

fn admin(m: &RunsMsg) -> Msg {
    Msg {
        target: "runs".into(),
        payload: encode_msg(m),
    }
}

fn engagement(channel: &str, seq: u64, _tags: Vec<()>) -> Msg {
    admin(&RunsMsg::RequestRun {
        agent_id: "bot".into(),
        channel_id: channel.into(),
        anchor_seq: seq,
        demands: BTreeMap::new(),
        skills: Vec::new(),
    })
}

/// the dispatch plane's next-block delivery for a run.
fn result_event(run_id: &str, outcome: Result<Vec<u8>, String>) -> Msg {
    Msg {
        target: "runs".into(),
        payload: dispatch::encode_delivery(&dispatch::Delivery::Result(ResultEvent {
            dispatch_id: dispatch_id_for(run_id),
            recipe_id: recipe_id_for("bot"),
            outcome,
        })),
    }
}

/// a jobs-board submit notification, spec + matching hash included.
fn jobs_event(job_id: &str, kind: &str, spec: &str) -> Msg {
    Msg {
        target: "runs".into(),
        payload: jobs_encode_event(&JobsEvent::Submitted {
            job_id: job_id.into(),
            kind: kind.into(),
            submitter: tasks::Party::Key(vec![1]),
            spec: spec.into(),
            spec_hash: job_spec_hash(spec.as_bytes()),
        }),
    }
}

fn exec(m: &mut RunsModule, ctx: &mut CaptureCtx, op: &Msg) -> Result<(), Error> {
    // These unit probes exercise composition and validation with configured
    // models. The real host suite owns queue timing and program authority.
    m.models = ctx.agents.clone();
    let previous: BTreeSet<_> = m
        .receipts
        .staged()
        .keys()
        .filter(|key| key.starts_with("action/body/"))
        .cloned()
        .collect();
    if ctx.env.origin == Origin::Module("jobs".into()) {
        let origin = std::mem::replace(&mut ctx.env.origin, Origin::Program(2));
        let result = block_on(m.on_jobs_event(ctx, &op.payload));
        ctx.env.origin = origin;
        return result;
    }
    block_on(m.execute(ctx, op))?;
    let prepared: Vec<_> = m
        .receipts
        .staged()
        .iter()
        .filter(|(id, _)| id.starts_with("action/body/") && !previous.contains(*id))
        .map(|(_, bytes)| {
            let request: super::action_requests::ActionRequest = sdk::wire::decode(bytes).unwrap();
            Msg {
                target: request.view.target,
                payload: sdk::wire::encode(&request.view.payload),
            }
        })
        .collect();
    for message in prepared {
        if let Ok(RunsMsg::ExecuteDelegation { .. }) = decode_msg(&message.payload) {
            let origin = std::mem::replace(&mut ctx.env.origin, Origin::Program(2));
            ctx.query_count.set(0);
            ctx.query_keys.borrow_mut().clear();
            let result = block_on(m.execute(ctx, &message));
            ctx.env.origin = origin;
            result?;
        } else {
            ctx.msgs.push(message);
        }
    }
    Ok(())
}

fn commit(m: &mut RunsModule) {
    block_on(m.commit_block()).unwrap();
}

fn abort(m: &mut RunsModule) {
    block_on(m.abort_block()).unwrap();
}

fn pending_runs(m: &RunsModule) -> Vec<PendingRun> {
    let reply = block_on(m.query(&encode_query(&RunsQuery::PendingRuns))).unwrap();
    match runs_decode_reply(&reply).unwrap() {
        RunsReply::PendingRuns(runs) => runs,
        other => panic!("unexpected reply: {other:?}"),
    }
}

fn get_pending(m: &RunsModule, run_id: &str) -> Option<PendingRun> {
    pending_runs(m).into_iter().find(|p| p.run_id == run_id)
}

fn recent_runs(m: &RunsModule) -> Vec<RunRecord> {
    let reply = block_on(m.query(&encode_query(&RunsQuery::RecentRuns))).unwrap();
    match runs_decode_reply(&reply).unwrap() {
        RunsReply::RecentRuns(runs) => runs,
        other => panic!("unexpected reply: {other:?}"),
    }
}

// ---- shared forge fixtures (the compose lane + the PR sink) -------------------

/// a committed tracker item as the "forge" GetItem arm serves it.
fn forge_item_detail(
    number: u64,
    kind: forge::ItemKind,
    title: &str,
    body: &str,
    branches: Option<(&str, &str)>,
) -> forge::ItemDetail {
    forge::ItemDetail {
        summary: forge::ItemSummary {
            number,
            kind,
            title: title.into(),
            state: forge::ItemState::Open,
            author: Party::Key(vec![1; 32]),
            created_at: 0,
            updated_at: 0,
        },
        body: body.into(),
        channel_id: format!("forge:app:{number}"),
        source_branch: branches.map(|(s, _)| s.to_string()),
        target_branch: branches.map(|(_, t)| t.to_string()),
        merge_oid: None,
        reviews: Vec::new(),
    }
}

fn forge_issue(number: u64, title: &str, body: &str) -> forge::ItemDetail {
    forge_item_detail(number, forge::ItemKind::Issue, title, body, None)
}

fn forge_pr(number: u64, title: &str, body: &str, src: &str, tgt: &str) -> forge::ItemDetail {
    forge_item_detail(number, forge::ItemKind::Pr, title, body, Some((src, tgt)))
}

/// a registry holding the one agent "bot".
fn bot_registry() -> Registry {
    registry(&["bot"])
}

/// the forge-lane module: forge + files + pages wired (the production wiring).
fn forge_module() -> RunsModule {
    module()
        .with_sink_forge("forge")
        .with_files_module("files")
        .with_pages_module("pages")
}

/// a committed page in preorder as the "pages" GetPage arm serves it: the
/// root (title), a paragraph, and one unchecked todo — enough surface for
/// the injection assertions. the root names itself as `page`.
fn page_blocks(page_id: &str, title: &str) -> Vec<pages::Block> {
    let block = |id: &str, parent: Option<&str>, kind, text: &str| pages::Block {
        author: pages::Party::System,
        id: id.into(),
        parent: parent.map(str::to_string),
        page: page_id.into(),
        kind,
        text: text.into(),
        marks: Vec::new(),
        checked: false,
        children: Vec::new(),
    };
    vec![
        block(page_id, None, pages::BlockKind::Page, title),
        block(
            "b-p",
            Some(page_id),
            pages::BlockKind::Paragraph,
            "spec paragraph",
        ),
        block("b-t", Some(page_id), pages::BlockKind::Todo, "do the thing"),
    ]
}

fn page_with_block_count(total: usize, text: &str) -> Vec<pages::Block> {
    assert!(total > 0);
    let child_ids = (1..total)
        .map(|index| format!("block-{index}"))
        .collect::<Vec<_>>();
    let mut blocks = vec![pages::Block {
        author: pages::Party::System,
        id: "plan".into(),
        parent: None,
        page: "plan".into(),
        kind: pages::BlockKind::Page,
        text: "Project Plan".into(),
        marks: Vec::new(),
        checked: false,
        children: child_ids.clone(),
    }];
    blocks.extend(child_ids.into_iter().map(|id| pages::Block {
        author: pages::Party::System,
        id,
        parent: Some("plan".into()),
        page: "plan".into(),
        kind: pages::BlockKind::Paragraph,
        text: text.into(),
        marks: Vec::new(),
        checked: false,
        children: Vec::new(),
    }));
    blocks
}

/// A module whose current model configuration matches the query fixture.
fn configured(registry: &Registry) -> RunsModule {
    let mut module = module();
    module.models = registry.clone();
    module
}

fn request_post(
    m: &mut RunsModule,
    registry: &Registry,
    seq: u64,
    requested: &[&str],
) -> CaptureCtx {
    let mut ctx = CaptureCtx::new()
        .at(seq)
        .with_registry(registry)
        .with_transcript("general", transcript(seq));
    let ids: Vec<String> = if requested.is_empty() {
        registry.keys().cloned().collect()
    } else {
        requested.iter().map(|id| (*id).into()).collect()
    };
    for id in ids {
        let Some(model) = registry.get(&id) else {
            continue;
        };
        if model.status != ModelStatus::Active {
            continue;
        }
        ctx.env.origin = Origin::Program(model.account);
        exec(
            m,
            &mut ctx,
            &admin(&RunsMsg::RequestRun {
                agent_id: id,
                channel_id: "general".into(),
                anchor_seq: seq,
                demands: BTreeMap::new(),
                skills: Vec::new(),
            }),
        )
        .unwrap();
    }
    ctx
}

/// build a faceted RunnerResult wrapper: the three core fields plus whatever
/// facet keys `facets` carries (sink / status, and a `workspace_receipt`
/// override when present).
fn runner_wrapper(response_text: &str, facets: serde_json::Value) -> Vec<u8> {
    let mut obj = serde_json::json!({
        "ducktape_runner_result": 1,
        "response_text": response_text,
        "workspace_receipt": {
            "source_prefix": "/shared/agent-workspaces/bot",
            "source_snapshot": null,
            "output_snapshot": null,
            "commit_height": null,
            "rebased": false,
            "no_changes": true
        }
    });
    if let serde_json::Value::Object(extra) = facets {
        let base = obj.as_object_mut().expect("object");
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    serde_json::to_vec(&obj).expect("wrapper serializes")
}

/// the model's strict-output prose (a bare AgentResponse JSON), wrapped in
/// the host-assembled runner result the oracle now ALWAYS delivers (the
/// marker-less flat tolerance is gone — flag day).
fn response(reply: &[&str], actions: Vec<ActionEnvelope>) -> Vec<u8> {
    let prose = String::from_utf8(response_json(reply, actions)).expect("utf-8");
    runner_wrapper(&prose, serde_json::json!({}))
}

/// the bare AgentResponse wire JSON — the PROSE inside [`response`], and the
/// expected-value shape assertions compare against.
fn response_json(reply: &[&str], actions: Vec<ActionEnvelope>) -> Vec<u8> {
    crate::encode_response(&AgentResponse {
        reply_blocks: reply
            .iter()
            .map(|t| ReplyBlock {
                kind: "paragraph".into(),
                text: (*t).into(),
                lang: None,
            })
            .collect(),
        actions,
        commit_message: None,
    })
}

/// a committed module holding one pending run for "bot" at general/2, plus
/// the registry and the run id.
fn awaiting_run() -> (RunsModule, Registry, String) {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    request_post(&mut m, &registry, 2, &[]);
    commit(&mut m);
    (m, registry, run_id_for("general", 2, "bot"))
}
/// the canned registry for the jobs lane: "duck".
fn job_registry() -> Registry {
    registry(&["duck"])
}
fn saga_view(key: &[u8], attempt: u32, status: saga::SagaStatus) -> saga::SagaView {
    saga::SagaView {
        origin: saga::SagaOrigin::Module("dispatch".into()),
        reply_to: Some("dispatch".into()),
        reply_payload: Vec::new(),
        spec: Vec::new(),
        capability: Some("model-1".into()),
        status,
        attempt,
        max_attempts: RUN_MAX_ATTEMPTS,
        assignee: Some(key.to_vec()),
        pinned_assignee: None,
        lease_views: None,
        lease_expires_at: None,
        deadline: None,
        result: Some(Vec::new()),
        error: None,
        created_at: 0,
        updated_at: 0,
    }
}

// ---- catalog envelopes ---------------------------------------------------------
// the operations tests submit, built the way an agent builds them: an
// operation name, an optional target, an input — never a typed variant.

fn envelope(
    operation: &str,
    target: Option<serde_json::Value>,
    input: serde_json::Value,
) -> ActionEnvelope {
    ActionEnvelope::new(operation, target, input)
}

fn text_content(text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"content": [{"type": "text", "text": text.into()}]})
}

fn reply(text: impl Into<String>) -> ActionEnvelope {
    envelope(crate::OP_REPLY, None, text_content(text))
}

fn react(emoji: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_REACT,
        None,
        serde_json::json!({"emoji": emoji.into()}),
    )
}

fn unreact(emoji: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_UNREACT,
        None,
        serde_json::json!({"emoji": emoji.into()}),
    )
}

fn post_message(
    channel_id: impl Into<String>,
    text: impl Into<String>,
    thread: Option<u64>,
) -> ActionEnvelope {
    let mut target = serde_json::json!({"channel_id": channel_id.into()});
    if let Some(root) = thread {
        target["thread"] = root.into();
    }
    envelope(OP_CHAT_POST_MESSAGE, Some(target), text_content(text))
}

fn page_comment(target: impl Into<String>, text: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_PAGES_COMMENT,
        Some(serde_json::json!({"target": target.into()})),
        text_content(text),
    )
}

fn page_thread_comment(thread_id: impl Into<String>, text: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_PAGES_COMMENT,
        Some(serde_json::json!({"thread_id": thread_id.into()})),
        text_content(text),
    )
}

fn page_post(title: impl Into<String>, content: serde_json::Value) -> ActionEnvelope {
    envelope(
        crate::OP_PAGES_POST,
        None,
        serde_json::json!({"title": title.into(), "content": content}),
    )
}

fn set_page_checked(block_id: impl Into<String>, checked: bool) -> ActionEnvelope {
    envelope(
        crate::OP_PAGES_SET_CHECKED,
        Some(serde_json::json!({"block_id": block_id.into()})),
        serde_json::json!({"checked": checked}),
    )
}

fn job_comment(job_id: impl Into<String>, text: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_JOBS_COMMENT,
        Some(serde_json::json!({"job_id": job_id.into()})),
        text_content(text),
    )
}

fn create_task(task_id: impl Into<String>, title: impl Into<String>) -> ActionEnvelope {
    envelope(
        OP_TASKS_CREATE,
        None,
        serde_json::json!({"task_id": task_id.into(), "title": title.into()}),
    )
}

fn update_task_status(task_id: impl Into<String>, status: impl Into<String>) -> ActionEnvelope {
    envelope(
        OP_TASKS_UPDATE_STATUS,
        Some(serde_json::json!({"task_id": task_id.into()})),
        serde_json::json!({"status": status.into()}),
    )
}

fn duckfs_write_text(
    path: impl Into<String>,
    text: impl Into<String>,
    base_snapshot: Option<String>,
) -> ActionEnvelope {
    let mut input = serde_json::json!({"text": text.into()});
    if let Some(base) = base_snapshot {
        input["base_snapshot"] = base.into();
    }
    envelope(
        crate::OP_DUCKFS_WRITE_TEXT,
        Some(serde_json::json!({"path": path.into()})),
        input,
    )
}

fn agent_call(agent_id: impl Into<String>, instruction: impl Into<String>) -> ActionEnvelope {
    envelope(
        crate::OP_AGENT_CALL,
        Some(serde_json::json!({"agent_id": agent_id.into()})),
        serde_json::json!({"instruction": instruction.into()}),
    )
}

fn forge_open_pr(
    repo: &str,
    source: &str,
    target: &str,
    title: &str,
    body: &str,
) -> ActionEnvelope {
    envelope(
        crate::OP_FORGE_OPEN_PR,
        Some(serde_json::json!({"repo": repo})),
        serde_json::json!({
            "source_branch": source,
            "target_branch": target,
            "title": title,
            "body": body,
        }),
    )
}

mod collaboration_actions;
mod composition;
mod delivery;
mod facets;
mod job_runs;
mod pages_actions;
mod receipts;
mod registry;
mod sessions;
mod state;
mod validation;
