// Loopback module state for the real-Pi product wiring smoke. This is NOT a
// consensus/network simulator: it checks module envelopes, CAS, retained Files
// roots, action receipts and readback. Chief decisions remain real package code.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

import { createNetworkRpc, createNetworkService } from "../../../../../../agents/chief/network.ts";
import type { NetworkToolAdapter } from "../../../../../../agents/chief/network.ts";
import { createNetworkPages } from "../../../../../../agents/chief/network-pages.ts";
import type { Board } from "../../../../../../agents/chief/contracts.ts";

const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
const canonical = (value: any): string => {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  return `{${Object.entries(value).filter(([, item]) => item !== undefined)
    .sort(([left], [right]) => Buffer.compare(Buffer.from(left), Buffer.from(right)))
    .map(([key, item]) => `${JSON.stringify(key)}:${canonical(item)}`).join(",")}}`;
};
const reply = (value: unknown) => ({ content: [{ type: "text", text: JSON.stringify(value) }], isError: false });

export const moduleFixture = (runId: () => string) => {
  const config = { agentId: "chief", conversationId: "fixture-conversation", boardPageId: "board", inboxPageId: "inbox", homePageId: "home", workerAgentId: "worker" };
  const state = {
    revision: 0, records: new Map<string, any>(), protected: new Map<string, any>(), receipts: new Map<string, any>(),
    files: new Map<string, Buffer>(), snapshots: new Map<string, Map<string, Buffer>>(), head: digest("empty"), fileRevision: 0,
    jobs: new Map<string, any>(), schedules: new Map<string, any>(), actions: new Map<string, number>(),
    actionReceipts: new Map<string, any>(),
    http: [] as Array<{ path: string; body: any }>, calls: [] as Array<{ name: string; args: any; runId: string }>,
  };
  const snapshot = (paths: Map<string, Buffer>): string => {
    const root = digest(canonical([...paths].sort(([left], [right]) => left.localeCompare(right)).map(([path, content]) => [path, content.toString("base64")])));
    state.snapshots.set(root, new Map(paths));
    return root;
  };
  const queryFiles = (input: any): unknown => {
    if (input.refs) return { refs: { head: state.head } };
    if (input.stat) return { stat: state.files.has(input.stat.path) ? {} : null };
    const read = input.read;
    const content = state.snapshots.get(read.snapshot)?.get(read.path);
    assert(content, "Files reads must use an existing root and original absolute path");
    return { read: { b64: content.subarray(read.offset, read.offset + read.len).toString("base64"), eof: read.offset + read.len >= content.length } };
  };
  const queryPages = (input: any): unknown => {
    const operation = Object.values(input)[0] as { page_id: string };
    assert.equal(operation.page_id, config.boardPageId);
    if (input.record_collection) return { record_collection: { page_id: config.boardPageId, writer: { account: 7 }, revision: state.revision, record_count: state.records.size } };
    if (input.record_state) return { record_state: structuredClone(state.protected.get(input.record_state.key) ?? null) };
    if (input.record_receipt) return { record_receipt: structuredClone(state.receipts.get(input.record_receipt.request_id) ?? null) };
    const page = input.records;
    assert.equal(page.limit, 32);
    const rows = [...state.records.values()].sort((a, b) => a.record_id.localeCompare(b.record_id)).filter(row => page.after === null || row.record_id > page.after);
    const records = rows.slice(0, page.limit);
    return { records: { revision: state.revision, records: structuredClone(records), next_after: rows.length > page.limit ? records.at(-1).record_id : null } };
  };
  const queryRuns = (input: any): unknown => {
    if (input === "agent_sessions") return { agent_sessions: [{ agent_id: config.agentId, run_id: runId(), actions: state.actions.get(runId()) ?? 0, session_key: Array(32).fill(241) }] };
    if (input.conversation) return { conversation: { conversation_id: config.conversationId, agent_id: config.agentId, account: 7, source: { channel: { channel_id: "shared" } } } };
    if (input.conversation_schedules) return { conversation_schedules: [...state.schedules.values()] };
    // The receipt lane a rejected effect is recovered through, keyed by the id
    // runs derives — the same id the action answered with.
    if (input.action_request) return { action_request: structuredClone(state.actionReceipts.get(input.action_request.request_id) ?? null) };
    assert.deepEqual(input, { model: { query: { agent: { agent_id: config.agentId } } } });
    return { model: { agent: { agent_id: config.agentId, account: 7, owner: { External: Array(32).fill(7) }, display_name: "Chief", capability: "pi", status: "active", role: "general", created_at: 0, updated_at: 0, skills: [], recipe_hash: [] } } };
  };
  const query = (module: string, input: any): unknown => {
    switch (module) {
      case "pages": return queryPages(input);
      case "files": return queryFiles(input);
      case "runs": return queryRuns(input);
      case "tasks": assert(input.job.get); return { job: { job: structuredClone(state.jobs.get(input.job.get.job_id) ?? null) } };
      default: throw new Error(`Unexpected fixture query: ${module}`);
    }
  };
  const applyPages = (input: any): number[] => {
    const commit = input.commit_records;
    assert.equal(commit.page_id, config.boardPageId);
    assert.equal(commit.expected_revision, state.revision);
    assert(commit.changes.length + commit.state_changes.length <= 16);
    assert(Buffer.byteLength(canonical(input)) <= 128 * 1024);
    assert(commit.artifacts.length <= 8 && commit.artifacts.includes(commit.metadata.history.hash));
    assert(commit.artifacts.every((root: string) => state.snapshots.has(root)), "Receipt retention requires existing immutable roots");
    const revision = state.revision + 1;
    commit.changes.forEach((change: any) => {
      if (change.delete) { state.records.delete(change.delete.record_id); return; }
      assert(["task", "ask", "rule"].includes(change.upsert.data.kind));
      state.records.set(change.upsert.record_id, { record_id: change.upsert.record_id, data: structuredClone(change.upsert.data), revision });
    });
    commit.state_changes.forEach((change: any) => {
      if (change.delete) { state.protected.delete(change.delete.key); return; }
      state.protected.set(change.put.key, { ...structuredClone(change.put), revision });
    });
    assert(state.records.size <= 1024 && state.protected.size <= 256);
    state.revision = revision;
    state.receipts.set(commit.request_id, { page_id: config.boardPageId, request_id: commit.request_id, revision,
      metadata: structuredClone(commit.metadata), artifacts: [...commit.artifacts], payload_digest: [...createHash("sha256").update(canonical(input)).digest()] });
    return [];
  };
  const applyFiles = (input: any): number[] => {
    state.fileRevision++;
    const output = (outcome: unknown): number[] => [...Buffer.from(JSON.stringify({ actor: { Account: 7 }, source_revision: state.fileRevision, outcome }))];
    if (input.commit) {
      input.commit.changes.forEach(({ put }: any) => state.files.set(put.path, Buffer.from(put.content.inline.b64, "base64")));
      state.head = snapshot(state.files);
      return output({ commit: { snapshot: state.head } });
    }
    const projection = input.project_snapshot;
    const content = state.snapshots.get(projection.snapshot)?.get(projection.path);
    assert(content);
    return output({ project_snapshot: { snapshot: snapshot(new Map([[projection.path, content]])) } });
  };
  const apply = (module: string, input: any): number[] => {
    switch (module) {
      case "pages": return applyPages(input);
      case "files": return applyFiles(input);
      case "tasks": {
        const job = input.job.submit_conversation;
        assert(job, "Only bootstrap fresh-worker dispatch is in this fixture's scope");
        assert(!state.jobs.has(job.job_id), "A worker must not be dispatched twice");
        assert.equal(job.kind, `agent/${config.workerAgentId}`);
        state.jobs.set(job.job_id, { job_id: job.job_id, conversation_id: `worker-${job.job_id}`, kind: job.kind, spec: job.spec, submitter: { account: 7 },
          execution: "conversation", created_at_revision: state.jobs.size + 1, status: "pending", attempt: 0, previous_job_id: null, reports: [], result: null });
        return [];
      }
      case "runs": {
        const schedule = input.schedule_conversation_input;
        assert(schedule);
        state.schedules.set(schedule.schedule_id, { ...schedule, status: schedule.after_secs === null ? "cancelled" : { pending: { due_at: schedule.after_secs } } });
        return [];
      }
      default: throw new Error(`Unexpected fixture action: ${module}`);
    }
  };
  const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
    signal?.throwIfAborted();
    state.calls.push({ name, args: structuredClone(args), runId: runId() });
    if (name === "ducktape_whoami") return reply({ agent_id: config.agentId, run_id: runId() });
    const module = (args.target as { module: string }).module;
    if (name === "ducktape_query") return reply(query(module, args.input));
    assert.equal(name, "ducktape_action");
    assert.equal(args.operation, "submit");
    assert.match(String(args.request_id), /^[a-f0-9]{64}$/);
    const actions = (state.actions.get(runId()) ?? 0) + 1;
    assert(actions <= 32, "Native action budget remains 32 per execution");
    state.actions.set(runId(), actions);
    const assigned = apply(module, args.input);
    // Runs answers under the id it derives, never the caller's own request id.
    const receiptId = `action/${digest(runId())}/${digest(String(args.request_id))}`;
    const receipt = { run_id: runId(), request_id: receiptId, account: 7, generation: 1, result: null,
      operation: "submit", target: module, payload: structuredClone(args.input),
      status: { completed: { call: { requester: "agent", invocation: runId(), step: actions }, outcome: { applied: { assigned, output_digest: Array(32).fill(0) } } } } };
    state.actionReceipts.set(receiptId, structuredClone(receipt));
    return reply({ receipt_id: receiptId, receipt });
  } };
  const node = async (path: string, body: any): Promise<unknown> => {
    state.http.push({ path, body: structuredClone(body) });
    if (path === "/v1/query") return query(body.target, body.query);
    assert.equal(path, "/v1/run-action");
    const action = body.message.agent_action;
    assert.equal(action.run_id, runId());
    const result = await adapter.callTool("ducktape_action", { ...action.action, request_id: action.request_id });
    return JSON.parse(result.content[0].text!);
  };
  const pages = createNetworkPages(createNetworkRpc(adapter), config.boardPageId, config.conversationId);
  return { config, state, node, start: () => createNetworkService(adapter, config),
    board: (): Promise<Board> => pages.read().then(snapshot => snapshot.value as Board) };
};
