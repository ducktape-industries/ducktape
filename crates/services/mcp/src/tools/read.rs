//! the read plane: everything an agent could previously only be TOLD, it can
//! ask for.
//!
//! before this plane existed a run saw exactly what the composer pre-injected
//! into its envelope — the anchored conversation, and a forge item's context if
//! it had one. it could not look up the task it was asked about, read the page
//! it was told to comment on, or open the sibling issue that explains the one it
//! is working. every one of those had to be foreseen in consensus, at compose
//! time, by code that could not know what the agent would want.
//!
//! the plane is four tools. `ducktape_whoami` answers who this run is.
//! `ducktape_actions` lists the catalog: the write operations the runs module
//! owns, straight from consensus, beside the read operations this binary
//! serves. `ducktape_query` runs one read operation by name with the same
//! `operation`/`target`/`input` envelope a write takes, and `ducktape_receipt`
//! reads a write's committed receipt back. reads cross no consensus op, so
//! their table lives here; writes are the module's, so their table does not.
//!
//! queries are built from each module's OWN `*Query` enum rather than
//! hand-written json, so a wire change in `chat` or `forge` breaks this file at
//! COMPILE time instead of at run time in front of a model.
//!
//! reads are not gated: a run reads what any member of the network reads. the
//! `query` operation is the floor under the typed table — any module's own
//! query, verbatim — so a read the table lacks a name for is still one call.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use forge::ForgeQuery;
use pages::PageQuery;
use runs::{ModelQuery, RunsQuery};
use tasks::{JobsQuery, TaskQuery, WorkQuery};

use super::{Tool, arg_str, opt_u64, schema};
use crate::identity::{Run, TARGET_MODEL, TARGET_RUNS};
use crate::node::{NodeError, Result};

const TARGET_CHAT: &str = "chat";
const TARGET_TASKS: &str = "tasks";
const TARGET_PAGES: &str = "pages";
const TARGET_FORGE: &str = "forge";
/// the generic read: any module's own query, verbatim.
pub const OP_QUERY: &str = "query";

/// the read-list default: enough context to be useful, small enough that a
/// careless call cannot blow the model's context.
const DEFAULT_READ_LIMIT: u64 = 50;
const MAX_READ_LIMIT: u64 = 200;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "ducktape_whoami",
            description: "Who you are in Ducktape: your run id, agent id, display name, owner, \
                          program account, your workspace directory, and where your skills are \
                          mounted. Call this first if you are unsure who you are acting as.",
            schema: || schema(&[]),
            handler: whoami,
        },
        Tool {
            name: "ducktape_actions",
            description: "The operation catalog. Each write operation comes from the runs module \
                          with its target and input schemas, the receipt result it reports and \
                          the lanes it admits (live via ducktape_action, \
                          final via your final response). Each read operation is one \
                          ducktape_query can run, with its target and input schemas. Pass filter \
                          to keep only names starting with it (e.g. \"pages.\").",
            schema: || {
                schema(&[(
                    "filter",
                    "string",
                    false,
                    "A name prefix to narrow the catalog.",
                )])
            },
            handler: actions,
        },
        Tool {
            name: "ducktape_query",
            description: "Run one read operation from the catalog: operation names it, target \
                          selects the resource it reads (omit when the operation takes none), \
                          input carries its options. The query operation runs any module's own \
                          query verbatim: target names the module, input is the query as that \
                          module's wire spells it (an object with one key, or a bare string). \
                          Reads are not gated.",
            schema: query_schema,
            handler: query,
        },
        Tool {
            name: "ducktape_receipt",
            description: "Read the committed receipt of one write by the receipt_id \
                          ducktape_action returned: its operation, result, target, payload and \
                          status (awaiting the program, claimed, completed with the target's \
                          outcome, or rejected with the reason).",
            schema: || {
                schema(&[(
                    "id",
                    "string",
                    true,
                    "The receipt_id ducktape_action returned.",
                )])
            },
            handler: receipt,
        },
    ]
}

/// one read operation: its catalog entry and the handler behind it. `target`
/// is `None` for an operation that reads no particular resource.
pub(super) struct ReadOperation {
    pub name: &'static str,
    pub description: &'static str,
    pub target: Option<Value>,
    pub input: Value,
    pub handler: fn(&Run, &Value, &Value) -> Result<Value>,
}

impl ReadOperation {
    fn view(&self) -> Value {
        json!({
            "kind": "read",
            "name": self.name,
            "description": self.description,
            "target": self.target,
            "input": self.input,
        })
    }
}

fn closed(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn no_input() -> Value {
    closed(json!({}), &[])
}

pub(super) fn read_operations() -> Vec<ReadOperation> {
    vec![
        ReadOperation {
            name: "agents.list",
            description: "List registered agents with their status, owner, capability and curated skills.",
            target: None,
            input: bounded_list_schema(),
            handler: agents_list,
        },
        ReadOperation {
            name: "runs.list",
            description: "List in-flight run correlations and this node's recent terminal run observations. Recent runs are a bounded derived cache and can be empty after a snapshot join. Live agent sessions and session keys are deliberately not exposed.",
            target: None,
            input: bounded_list_schema(),
            handler: runs_list,
        },
        ReadOperation {
            name: "chat.channels",
            description: "List every chat channel, with its id and name.",
            target: None,
            input: no_input(),
            handler: chat_channels,
        },
        ReadOperation {
            name: "chat.messages",
            description: "Read the most recent top-level messages of a chat channel, oldest first. Each root carries its thread summary.",
            target: Some(closed(
                json!({"channel_id": {"type": "string"}}),
                &["channel_id"],
            )),
            input: closed(
                json!({"limit": {"type": "integer", "description": "How many of the newest roots to return (default 50, max 200)."}}),
                &[],
            ),
            handler: chat_messages,
        },
        ReadOperation {
            name: "tasks.list",
            description: "Read one bounded page of tasks — id, title and status (open, in_progress, done) — in ascending id order. Pass the last id you saw as after to continue.",
            target: None,
            input: tasks_list_schema(),
            handler: tasks_list,
        },
        ReadOperation {
            name: "jobs.get",
            description: "Read a job's specification, execution status, result and bounded discussion, including each comment's authenticated author.",
            target: Some(closed(json!({"job_id": {"type": "string"}}), &["job_id"])),
            input: no_input(),
            handler: job_get,
        },
        ReadOperation {
            name: "pages.list",
            description: "Read one bounded page of page ids and titles. Pass next_after as after to continue.",
            target: None,
            input: page_cursor_schema(),
            handler: pages_list,
        },
        ReadOperation {
            name: "pages.get",
            description: "Read one bounded document-order block page. Pass next_after as after to continue. Block ids here are what pages.comment and pages.set_checked target.",
            target: Some(closed(json!({"page_id": {"type": "string"}}), &["page_id"])),
            input: page_cursor_schema(),
            handler: page_get,
        },
        ReadOperation {
            name: "forge.repos",
            description: "List the forge repos and their current heads.",
            target: None,
            input: no_input(),
            handler: forge_repos,
        },
        ReadOperation {
            name: "forge.items",
            description: "List a forge repo's issues and pull requests.",
            target: Some(closed(json!({"repo": {"type": "string"}}), &["repo"])),
            input: no_input(),
            handler: forge_items,
        },
        ReadOperation {
            name: "forge.item",
            description: "Read one forge issue or pull request in full — body, branches, reviews, and the id of its discussion channel (readable with chat.messages).",
            target: Some(closed(
                json!({"repo": {"type": "string"}, "number": {"type": "integer"}}),
                &["repo", "number"],
            )),
            input: no_input(),
            handler: forge_item,
        },
        ReadOperation {
            name: "forge.pr_diff",
            description: "Read a pull request's exact committed source and target OIDs plus a bounded unified patch and full diff statistics. The patch is capped at 48 KiB and reports truncation; inputs beyond 256 changed files or 8 MiB of aggregate blobs fail instead of returning partial statistics. Fails if the item is not a PR or the pinned git objects are unavailable locally.",
            target: Some(closed(
                json!({"repo": {"type": "string"}, "number": {"type": "integer"}}),
                &["repo", "number"],
            )),
            input: no_input(),
            handler: forge_pr_diff,
        },
        ReadOperation {
            name: "files.ls",
            description: "List a directory in the Ducktape filesystem (duckfs). This is the shared, replicated filesystem — NOT your local workspace, which you read with ordinary file tools.",
            target: Some(closed(json!({"path": {"type": "string"}}), &["path"])),
            input: no_input(),
            handler: files_ls,
        },
        ReadOperation {
            name: "files.read",
            description: "Read a file from the Ducktape filesystem (duckfs) as text.",
            target: Some(closed(json!({"path": {"type": "string"}}), &["path"])),
            input: no_input(),
            handler: files_read,
        },
        ReadOperation {
            name: "files.grep",
            description: "Search the Ducktape filesystem (duckfs) for matching lines under a path prefix.",
            target: Some(closed(json!({"prefix": {"type": "string"}}), &["prefix"])),
            input: closed(json!({"pattern": {"type": "string"}}), &["pattern"]),
            handler: files_grep,
        },
        ReadOperation {
            name: "agent.calls",
            description: "List this run's agent.call edges: each pending call and every delivered, failed or cancelled result.",
            target: None,
            input: no_input(),
            handler: agent_calls,
        },
        ReadOperation {
            name: OP_QUERY,
            description: "Run any module's own query verbatim — the floor under this table. target names the module; input is the query exactly as that module's wire spells it: an object with exactly one key (the query name) or a bare string. The module's reply comes back untouched, and an unknown query name is refused with the names the module does accept.",
            target: Some(closed(json!({"module": {"type": "string"}}), &["module"])),
            input: json!({
                "oneOf": [
                    {"type": "object", "minProperties": 1, "maxProperties": 1},
                    {"type": "string"},
                ],
            }),
            handler: module_query,
        },
    ]
}

fn find_read(name: &str) -> Option<ReadOperation> {
    read_operations().into_iter().find(|op| op.name == name)
}

fn query_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "description": "A read operation name from ducktape_actions.",
            },
            "target": {
                "type": "object",
                "description": "The resource to read, per the operation's target schema. Omit for operations that take none.",
            },
            "input": {
                "type": "object",
                "description": "The operation's options, per its input schema. Omit when it has none.",
            },
        },
        "required": ["operation"],
        "additionalProperties": false,
    })
}

/// the agent's own committed record, plus the host facts it cannot read off the
/// chain: its run id, workspace, and skill mount.
fn whoami(run: &Run, _args: &Value) -> Result<Value> {
    let record = run.record()?;
    Ok(json!({
        "account": record.account,
        "agent_id": record.agent_id,
        "display_name": record.display_name,
        "owner": record.owner,
        "capability": record.capability,
        "status": record.status,
        "skills": record.skills,
        "run_id": run.run_id(),
        "workspace_dir": run.workspace,
        "skills_dir": run.skills,
    }))
}

/// the write catalog as consensus holds it, beside the read table this binary
/// serves. the writes are fetched per call: a module swap that adds an
/// operation shows up here without a tool-binary update.
fn actions(run: &Run, args: &Value) -> Result<Value> {
    let filter = match args.get("filter") {
        None | Some(Value::Null) => None,
        Some(Value::String(filter)) => Some(filter.clone()),
        Some(_) => {
            return Err(NodeError::Rejected(
                "this tool needs a string \"filter\" argument when one is given".into(),
            ));
        }
    };
    let reply = run.node.query(
        TARGET_RUNS,
        encode(&RunsQuery::Catalog {
            filter: filter.clone(),
        })?,
    )?;
    let mut operations: Vec<Value> = reply_array(&reply, "catalog")?
        .into_iter()
        .map(|mut view| {
            view["kind"] = json!("write");
            view
        })
        .collect();
    let keep = |name: &str| {
        filter
            .as_deref()
            .is_none_or(|prefix| name.starts_with(prefix))
    };
    operations.extend(
        read_operations()
            .iter()
            .filter(|op| keep(op.name))
            .map(ReadOperation::view),
    );
    Ok(json!({"operations": operations}))
}

/// one read operation by name. the envelope is checked for shape only — an
/// object target where the operation takes one, none where it takes none —
/// and each handler reads its own fields by name so a refusal names them.
fn query(run: &Run, args: &Value) -> Result<Value> {
    let name = arg_str(args, "operation")?;
    let Some(operation) = find_read(&name) else {
        return Err(NodeError::Rejected(format!(
            "{name:?} is not a read operation; ducktape_actions lists them"
        )));
    };
    let target = match (args.get("target"), operation.target.is_some()) {
        (None | Some(Value::Null), false) => Value::Null,
        (None | Some(Value::Null), true) => {
            return Err(NodeError::Rejected(format!("{name} requires a target")));
        }
        (Some(_), false) => {
            return Err(NodeError::Rejected(format!("{name} takes no target")));
        }
        (Some(target @ Value::Object(_)), true) => target.clone(),
        (Some(_), true) => {
            return Err(NodeError::Rejected(format!(
                "{name} needs an object \"target\" argument"
            )));
        }
    };
    let generic = operation.name == OP_QUERY;
    let input = match (args.get("input"), generic) {
        (None | Some(Value::Null), _) => json!({}),
        (Some(input @ Value::Object(_)), _) => input.clone(),
        (Some(input @ Value::String(_)), true) => input.clone(),
        (Some(_), true) => {
            return Err(NodeError::Rejected(format!(
                "{name} needs an \"input\" argument that is the module's own query: an object \
                 with one key, or a string"
            )));
        }
        (Some(_), false) => {
            return Err(NodeError::Rejected(format!(
                "{name} needs an object \"input\" argument"
            )));
        }
    };
    (operation.handler)(run, &target, &input)
}

fn receipt(run: &Run, args: &Value) -> Result<Value> {
    run.node.query(
        TARGET_RUNS,
        encode(&RunsQuery::ActionRequest {
            request_id: arg_str(args, "id")?,
        })?,
    )
}

fn agents_list(run: &Run, _target: &Value, input: &Value) -> Result<Value> {
    let limit = list_limit(input)?;
    let reply = run.node.query(
        TARGET_MODEL,
        encode(&runs::RunsQuery::Model {
            query: ModelQuery::Agents,
        })?,
    )?;
    let (agents, total, truncated) = bounded(
        reply_array(
            reply
                .get("model")
                .ok_or_else(|| NodeError::Transport("missing model reply".into()))?,
            "agents",
        )?,
        limit,
    );
    Ok(json!({
        "agents": agents,
        "total": total,
        "truncated": truncated,
    }))
}

fn runs_list(run: &Run, _target: &Value, input: &Value) -> Result<Value> {
    let limit = list_limit(input)?;
    let pending = run
        .node
        .query(TARGET_RUNS, encode(&RunsQuery::PendingRuns)?)?;
    let recent = run
        .node
        .query(TARGET_RUNS, encode(&RunsQuery::RecentRuns)?)?;
    let (pending_runs, pending_total, pending_truncated) =
        bounded(reply_array(&pending, "pending_runs")?, limit);
    let (recent_runs, recent_total, recent_truncated) =
        bounded(reply_array(&recent, "recent_runs")?, limit);
    Ok(json!({
        "pending_runs": pending_runs,
        "pending_total": pending_total,
        "pending_truncated": pending_truncated,
        "recent_runs": recent_runs,
        "recent_total": recent_total,
        "recent_truncated": recent_truncated,
    }))
}

fn chat_channels(run: &Run, _target: &Value, _input: &Value) -> Result<Value> {
    run.node.view(TARGET_CHAT, json!({"channels": {}}))
}

fn chat_messages(run: &Run, target: &Value, input: &Value) -> Result<Value> {
    let limit = opt_u64(input, "limit")
        .unwrap_or(DEFAULT_READ_LIMIT)
        .min(MAX_READ_LIMIT);
    let query = json!({"roots": {
        "channel_id": arg_str(target, "channel_id")?,
        "limit": limit,
    }});
    run.node.view(TARGET_CHAT, query)
}

fn tasks_list(run: &Run, _target: &Value, input: &Value) -> Result<Value> {
    // the board's page bound is its own (`tasks::MAX_LIST_LIMIT`, 256) and it
    // clamps whatever arrives; the pages cursor/limit parsing carries the same
    // shape and the same 1..=256 range, so it is reused verbatim.
    let query = WorkQuery::Task(TaskQuery::List {
        limit: u64::from(page_limit(input)?),
        after: page_cursor(input)?,
    });
    run.node.query(TARGET_TASKS, encode(&query)?)
}

fn job_get(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let query = WorkQuery::Job(JobsQuery::Get {
        job_id: arg_str(target, "job_id")?,
    });
    run.node.query(TARGET_TASKS, encode(&query)?)
}

fn pages_list(run: &Run, _target: &Value, input: &Value) -> Result<Value> {
    run.node.view(
        TARGET_PAGES,
        json!({"list_pages": {"after": page_cursor(input)?, "limit": page_limit(input)?}}),
    )
}

fn page_get(run: &Run, target: &Value, input: &Value) -> Result<Value> {
    let query = PageQuery::GetPage {
        page_id: arg_str(target, "page_id")?,
        after: page_cursor(input)?,
        limit: page_limit(input)?,
    };
    run.node.query(TARGET_PAGES, encode(&query)?)
}

fn forge_repos(run: &Run, _target: &Value, _input: &Value) -> Result<Value> {
    run.node
        .query(TARGET_FORGE, encode(&ForgeQuery::ListRepos)?)
}

fn forge_items(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let repo = arg_str(target, "repo")?;
    let query = ForgeQuery::ListItems { repo };
    run.node.query(TARGET_FORGE, encode(&query)?)
}

fn forge_item(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let repo = arg_str(target, "repo")?;
    let number = item_number(target)?;
    let query = ForgeQuery::GetItem { repo, number };
    run.node.query(TARGET_FORGE, encode(&query)?)
}

fn forge_pr_diff(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let repo = arg_str(target, "repo")?;
    let number = item_number(target)?;
    let query = ForgeQuery::PrDiff { repo, number };
    run.node.query(TARGET_FORGE, encode(&query)?)
}

fn item_number(target: &Value) -> Result<u64> {
    opt_u64(target, "number").ok_or_else(|| {
        NodeError::Rejected("this operation needs an integer \"number\" in its target".into())
    })
}

fn files_ls(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let path = arg_str(target, "path")?;
    run.node.files("ls", &[("path", path)])
}

/// duckfs reads come back base64 in `b64`. an agent wants TEXT — hand it the
/// decoded body and say plainly when the bytes are not text, rather than
/// handing a model a base64 blob to decode in its head.
fn files_read(run: &Run, target: &Value, _input: &Value) -> Result<Value> {
    let path = arg_str(target, "path")?;
    let reply = run.node.files("read", &[("path", path.clone())])?;
    let Some(b64) = reply.get("b64").and_then(Value::as_str) else {
        return Ok(reply);
    };
    let bytes = STANDARD
        .decode(b64)
        .map_err(|e| NodeError::Transport(format!("duckfs returned undecodable base64: {e}")))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(json!({
            "path": path,
            "text": text,
            "eof": reply.get("eof").cloned().unwrap_or(Value::Null),
        })),
        Err(e) => Err(NodeError::Rejected(format!(
            "{path:?} is not utf-8 text ({} bytes); this operation reads text files only",
            e.into_bytes().len()
        ))),
    }
}

fn files_grep(run: &Run, target: &Value, input: &Value) -> Result<Value> {
    let prefix = arg_str(target, "prefix")?;
    let pattern = arg_str(input, "pattern")?;
    run.node
        .files("grep", &[("pattern", pattern), ("prefix", prefix)])
}

fn agent_calls(run: &Run, _target: &Value, _input: &Value) -> Result<Value> {
    let run_id = run.run_id().ok_or_else(|| {
        NodeError::Rejected("this server is not bound to a run, so it has no agent calls".into())
    })?;
    run.node.query(
        TARGET_RUNS,
        encode(&RunsQuery::Delegations {
            caller_run_id: run_id.into(),
        })?,
    )
}

/// the generic read: the module's own query, verbatim, and its reply the same
/// way. the target module decides what the bytes mean; an unknown query name
/// comes back as that module's own refusal, naming the queries it accepts.
fn module_query(run: &Run, target: &Value, input: &Value) -> Result<Value> {
    let module = arg_str(target, "module")?;
    if module.is_empty() {
        return Err(NodeError::Rejected(
            "query needs a non-empty module in its target".into(),
        ));
    }
    let names_one_query = match input {
        Value::Object(fields) => fields.len() == 1,
        Value::String(name) => !name.is_empty(),
        _ => false,
    };
    if !names_one_query {
        return Err(NodeError::Rejected(
            "query needs the module's own query as input: an object with exactly one key, or a \
             non-empty string"
                .into(),
        ));
    }
    run.node.query(&module, input.clone())
}

fn bounded_list_schema() -> Value {
    let mut value = schema(&[(
        "limit",
        "integer",
        false,
        "Maximum rows to return (default 50, minimum 1, maximum 200).",
    )]);
    value["properties"]["limit"]["minimum"] = json!(1);
    value["properties"]["limit"]["maximum"] = json!(MAX_READ_LIMIT);
    value["properties"]["limit"]["default"] = json!(DEFAULT_READ_LIMIT);
    value["additionalProperties"] = Value::Bool(false);
    value
}

/// the task board's page args. same shape and same 1..=256 bound as the pages
/// reader, but the cursor is a task ID (the last one of the previous page), not
/// a `next_after` the reply carries.
fn tasks_list_schema() -> Value {
    let mut value = schema(&[
        (
            "after",
            "string",
            false,
            "Exclusive cursor: the last task id of the previous page.",
        ),
        (
            "limit",
            "integer",
            false,
            "Tasks to return (default and maximum 256).",
        ),
    ]);
    value["properties"]["limit"]["minimum"] = json!(1);
    value["properties"]["limit"]["maximum"] = json!(tasks::MAX_LIST_LIMIT);
    value["properties"]["limit"]["default"] = json!(tasks::MAX_LIST_LIMIT);
    value["additionalProperties"] = Value::Bool(false);
    value
}

fn page_cursor_schema() -> Value {
    let mut value = schema(&[
        (
            "after",
            "string",
            false,
            "Exclusive cursor from the prior page's next_after.",
        ),
        (
            "limit",
            "integer",
            false,
            "Records to return (default and maximum 256).",
        ),
    ]);
    value["properties"]["limit"]["minimum"] = json!(1);
    value["properties"]["limit"]["maximum"] = json!(pages::MAX_PAGE_QUERY_LIMIT);
    value["properties"]["limit"]["default"] = json!(pages::MAX_PAGE_QUERY_LIMIT);
    value["additionalProperties"] = Value::Bool(false);
    value
}

fn page_cursor(args: &Value) -> Result<Option<String>> {
    match args.get("after") {
        None => Ok(None),
        Some(Value::String(cursor)) => Ok(Some(cursor.clone())),
        Some(_) => Err(NodeError::Rejected(
            "this operation needs a string \"after\" argument".into(),
        )),
    }
}

fn page_limit(args: &Value) -> Result<u16> {
    let Some(value) = args.get("limit") else {
        return Ok(pages::MAX_PAGE_QUERY_LIMIT);
    };
    let limit = value.as_u64().ok_or_else(|| {
        NodeError::Rejected("this operation needs an integer \"limit\" argument".into())
    })?;
    if !(1..=u64::from(pages::MAX_PAGE_QUERY_LIMIT)).contains(&limit) {
        return Err(NodeError::Rejected(format!(
            "this operation needs \"limit\" between 1 and {}",
            pages::MAX_PAGE_QUERY_LIMIT
        )));
    }
    Ok(limit as u16)
}

fn list_limit(args: &Value) -> Result<usize> {
    let object = args
        .as_object()
        .ok_or_else(|| NodeError::Rejected("this operation needs an object input".into()))?;
    if object.keys().any(|key| key != "limit") {
        return Err(NodeError::Rejected(
            "this operation accepts only an optional integer \"limit\" argument".into(),
        ));
    }
    let limit = match object.get("limit") {
        None => DEFAULT_READ_LIMIT,
        Some(value) => value.as_u64().ok_or_else(|| {
            NodeError::Rejected("this operation needs an integer \"limit\" argument".into())
        })?,
    };
    if !(1..=MAX_READ_LIMIT).contains(&limit) {
        return Err(NodeError::Rejected(format!(
            "this operation needs \"limit\" between 1 and {MAX_READ_LIMIT}"
        )));
    }
    Ok(limit as usize)
}

fn reply_array(reply: &Value, name: &str) -> Result<Vec<Value>> {
    reply
        .get(name)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| NodeError::Transport(format!("module returned no {name:?} array: {reply}")))
}

fn bounded(mut rows: Vec<Value>, limit: usize) -> (Vec<Value>, usize, bool) {
    let total = rows.len();
    rows.truncate(limit);
    (rows, total, total > limit)
}

/// a module's own query enum as the json `/v1/query` carries. the round-trip
/// through `to_value` is what keeps this file honest: the enum, not a string
/// literal here, defines the wire.
fn encode<Q: serde::Serialize>(query: &Q) -> Result<Value> {
    serde_json::to_value(query)
        .map_err(|e| NodeError::Transport(format!("could not encode the query: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_encode_to_the_modules_own_wire_shapes() {
        // the guard against this file drifting from the module interfaces: if
        // chat renames a variant, this fails here rather than in front of a
        // model. chat's tools speak the index-tier view wire, so their json
        // literals must DECODE as chat's own view enum.
        serde_json::from_value::<chat::index::ChatViewQuery>(json!({"channels": {}}))
            .expect("the channels view literal is chat's view wire");
        serde_json::from_value::<chat::index::ChatViewQuery>(
            json!({"roots": {"channel_id": "c", "limit": 5}}),
        )
        .expect("the roots view literal is chat's view wire");
        assert_eq!(encode(&ModelQuery::Agents).unwrap(), json!("agents"));
        assert_eq!(
            encode(&RunsQuery::PendingRuns).unwrap(),
            json!("pending_runs")
        );
        assert_eq!(
            encode(&RunsQuery::RecentRuns).unwrap(),
            json!("recent_runs")
        );
        assert_eq!(
            encode(&RunsQuery::Catalog {
                filter: Some("pages.".into())
            })
            .unwrap(),
            json!({"catalog": {"filter": "pages."}})
        );
        assert_eq!(
            encode(&ForgeQuery::PrDiff {
                repo: "app".into(),
                number: 8,
            })
            .unwrap(),
            json!({"pr_diff": {"repo": "app", "number": 8}})
        );
        assert_eq!(
            encode(&WorkQuery::Task(TaskQuery::List {
                limit: 8,
                after: Some("t-3".into()),
            }))
            .unwrap(),
            json!({"task": {"list": {"limit": 8, "after": "t-3"}}})
        );
        serde_json::from_value::<pages::index::PagesViewQuery>(
            json!({"list_pages": {"after": "page-8", "limit": 8}}),
        )
        .expect("the list_pages view literal is pages' view wire");
        assert_eq!(
            encode(&ForgeQuery::GetItem {
                repo: "app".into(),
                number: 7,
            })
            .unwrap(),
            json!({"get_item": {"repo": "app", "number": 7}})
        );
    }

    #[test]
    fn read_operations_are_uniquely_named_and_disjoint_from_the_write_catalog() {
        let ops = read_operations();
        let mut names: Vec<&str> = ops.iter().map(|op| op.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "read operation names must be unique");
        for op in &ops {
            assert!(!op.description.is_empty(), "{} has no description", op.name);
            let input_is_an_object = op.input["type"] == "object";
            let input_is_the_modules_own_query = op.name == OP_QUERY;
            assert!(
                input_is_an_object || input_is_the_modules_own_query,
                "{} input is not an object",
                op.name
            );
            if let Some(target) = &op.target {
                assert_eq!(
                    target["type"], "object",
                    "{} target is not an object",
                    op.name
                );
            }
            assert!(
                runs::catalog(None)
                    .iter()
                    .all(|write| write.name != op.name),
                "{} collides with a write operation",
                op.name
            );
        }
    }

    #[test]
    fn the_query_envelope_is_checked_for_shape_before_any_handler_runs() {
        let run = Run::from_env();
        for (args, needle) in [
            (json!({}), "operation"),
            (json!({"operation": "nope"}), "not a read operation"),
            (json!({"operation": "jobs.get"}), "requires a target"),
            (
                json!({"operation": "chat.channels", "target": {"x": 1}}),
                "takes no target",
            ),
            (
                json!({"operation": "jobs.get", "target": "job-1"}),
                "object \"target\"",
            ),
            (
                json!({"operation": "tasks.list", "input": []}),
                "object \"input\"",
            ),
            (json!({"operation": "jobs.get", "target": {}}), "job_id"),
        ] {
            let error = query(&run, &args).unwrap_err();
            assert!(
                matches!(&error, NodeError::Rejected(m) if m.contains(needle)),
                "{args} -> {error:?}"
            );
        }
    }

    #[test]
    fn the_message_limit_is_defaulted_and_clamped() {
        let clamp = |v: Value| {
            opt_u64(&v, "limit")
                .unwrap_or(DEFAULT_READ_LIMIT)
                .min(MAX_READ_LIMIT)
        };
        assert_eq!(clamp(json!({})), DEFAULT_READ_LIMIT);
        assert_eq!(clamp(json!({"limit": 10})), 10);
        // a model that asks for the whole channel does not get to blow its own
        // context: the cap is ours, not its.
        assert_eq!(clamp(json!({"limit": 10_000})), MAX_READ_LIMIT);
    }

    #[test]
    fn page_cursor_and_limit_are_bounded() {
        assert_eq!(page_cursor(&json!({})).unwrap(), None);
        assert_eq!(
            page_cursor(&json!({"after": "b8"})).unwrap(),
            Some("b8".into())
        );
        assert!(matches!(
            page_cursor(&json!({"after": 8})),
            Err(NodeError::Rejected(_))
        ));
        assert_eq!(page_limit(&json!({})).unwrap(), pages::MAX_PAGE_QUERY_LIMIT);
        assert_eq!(page_limit(&json!({"limit": 3})).unwrap(), 3);
        assert!(matches!(
            page_limit(&json!({"limit": 0})),
            Err(NodeError::Rejected(_))
        ));
        assert_eq!(page_limit(&json!({"limit": 99})).unwrap(), 99);
        assert!(matches!(
            page_limit(&json!({"limit": 257})),
            Err(NodeError::Rejected(_))
        ));
    }

    #[test]
    fn pages_operations_expose_the_bounded_cursor() {
        for name in ["pages.list", "pages.get"] {
            let op = find_read(name).unwrap();
            assert_eq!(op.input["properties"]["after"]["type"], "string");
            assert_eq!(
                op.input["properties"]["limit"]["maximum"],
                pages::MAX_PAGE_QUERY_LIMIT
            );
            assert_eq!(op.input["additionalProperties"], false);
        }
    }

    #[test]
    fn agent_and_run_inputs_are_exactly_bounded() {
        let expected = json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum rows to return (default 50, minimum 1, maximum 200).",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 50,
                }
            },
            "required": [],
            "additionalProperties": false,
        });
        for name in ["agents.list", "runs.list"] {
            let op = find_read(name).unwrap();
            assert_eq!(op.target, None, "{name}");
            assert_eq!(op.input, expected, "{name}");
        }
    }

    #[test]
    fn agent_and_run_limits_reject_bad_arguments_before_querying() {
        let bad = [
            Value::Null,
            json!([]),
            json!("not an object"),
            json!({"other": 1}),
            json!({"limit": "1"}),
            json!({"limit": -1}),
            json!({"limit": 0}),
            json!({"limit": 201}),
        ];
        for input in bad {
            for handler in [
                agents_list as fn(&Run, &Value, &Value) -> Result<Value>,
                runs_list,
            ] {
                assert!(
                    matches!(
                        handler(&Run::from_env(), &Value::Null, &input),
                        Err(NodeError::Rejected(_))
                    ),
                    "accepted {input}"
                );
            }
        }
        assert_eq!(list_limit(&json!({})).unwrap(), 50);
        assert_eq!(list_limit(&json!({"limit": 1})).unwrap(), 1);
        assert_eq!(list_limit(&json!({"limit": 200})).unwrap(), 200);
    }

    #[test]
    fn bounded_rows_preserve_order_and_report_the_full_total() {
        let (rows, total, truncated) = bounded(vec![json!("first"), json!("second")], 1);
        assert_eq!(rows, vec![json!("first")]);
        assert_eq!(total, 2);
        assert!(truncated);
    }

    #[test]
    fn a_missing_required_argument_names_itself() {
        let err = arg_str(&json!({}), "channel_id").unwrap_err();
        assert!(
            matches!(&err, NodeError::Rejected(m) if m.contains("channel_id")),
            "got {err:?}"
        );
    }

    #[test]
    fn the_generic_query_takes_the_modules_own_query_and_nothing_else() {
        let run = Run::from_env();
        for (args, needle) in [
            (
                json!({"operation": OP_QUERY, "target": {"module": "forge"}}),
                "module's own query",
            ),
            (
                json!({"operation": OP_QUERY, "target": {"module": "forge"}, "input": {"a": 1, "b": 2}}),
                "exactly one key",
            ),
            (
                json!({"operation": OP_QUERY, "target": {"module": "forge"}, "input": ""}),
                "non-empty string",
            ),
            (
                json!({"operation": OP_QUERY, "target": {"module": "forge"}, "input": 7}),
                "object with one key, or a string",
            ),
            (
                json!({"operation": OP_QUERY, "target": {"module": ""}, "input": "list_repos"}),
                "non-empty module",
            ),
            // a bare string is the generic operation's shape alone
            (
                json!({"operation": "tasks.list", "input": "list"}),
                "object \"input\"",
            ),
        ] {
            let error = query(&run, &args).unwrap_err();
            assert!(
                matches!(&error, NodeError::Rejected(m) if m.contains(needle)),
                "{args} -> {error:?}"
            );
        }
        // a well-shaped query is the module's to judge, never this table's: it
        // reaches the node (or fails to, unbound) without a shape complaint.
        for input in [json!("list_repos"), json!({"list_items": {"repo": "app"}})] {
            let args =
                json!({"operation": OP_QUERY, "target": {"module": "forge"}, "input": input});
            if let Err(NodeError::Rejected(message)) = query(&run, &args) {
                panic!("{args} was refused for its shape: {message}");
            }
        }
    }
}
