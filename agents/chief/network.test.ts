// Network envelopes are checked against the actual Rust source constraints and
// MCP ToolsCallResult shape, not a parallel Chief-specific transport protocol.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import type { Board, ChiefCommand, EffectReceipt, FileRef, OperationMetadata } from './contracts.ts';
import type { CommittedConversationEvent } from './inputs.ts';
import { createNetworkPages } from './network-pages.ts';
import type { NetworkConfig, NetworkToolAdapter } from './network.ts';
import { createNetworkRpc, createNetworkService, MAX_NATIVE_ACTIONS, networkRequestId, reportPath } from './network.ts';

const reply = (value: unknown) => ({ content: [{ type: 'text', text: JSON.stringify(value) }], isError: false });

test('evidence artifacts live where Files actually authorizes a write', () => {
  // Files admits only `/home/<label>/**` and `/shared/**`; anything else is
  // refused at the path gate before authorization, so every Chief artifact
  // read, projection and publication must share this one shared-namespace root.
  const path = reportPath(`report-${'a'.repeat(64)}`);
  assert.match(path, /^\/shared\/[^/]+/);
  assert.equal(path, `/shared/agents/chief/reports/report-${'a'.repeat(64)}.txt`);
});

test('action request IDs respect the actual Runs64 byte catalog limit', async () => {
  const rust = await readFile(new URL('../../crates/modules/apps/runs/src/interface.rs', import.meta.url), 'utf8');
  const match = rust.match(/pub const MAX_REQUEST_ID_BYTES: usize = (\d+);/);
  assert.ok(match, 'source constraint must remain discoverable');
  const max = Number(match[1]);
  assert.equal(max, 64);
  assert.equal(Buffer.byteLength(networkRequestId('x'.repeat(160))), max);
  assert.equal(networkRequestId('same'), networkRequestId('same'));
  assert.notEqual(networkRequestId('same'), networkRequestId('different'));
});

test('generic catalog calls preserve module wire and authenticate only through adapter', async () => {
  const calls: { name: string; args: Record<string, unknown>; signal?: AbortSignal }[] = [];
  const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
    calls.push({ name, args, signal });
    if (name === 'ducktape_query') return reply({ record_collection: null });
    const receiptId = `action/${digest('test-run')}/${digest(String(args.request_id))}`;
    return reply({ receipt_id: receiptId, receipt: { request_id: receiptId, operation: 'submit', run_id: 'test-run', target: 'pages', payload: args.input, status: { completed: { outcome: { applied: { assigned: [], output_digest: Array(32).fill(0) } } } } } });
  } };
  const rpc = createNetworkRpc(adapter);
  const signal = new AbortController().signal;
  assert.deepEqual(await rpc.query('pages', { record_collection: { page_id: 'board' } }, signal), { record_collection: null });
  await rpc.submit('pages', { commit_records: { page_id: 'board', expected_revision: 0, request_id: 'page-operation', changes: [] } }, 'product-operation', signal);
  assert.deepEqual(calls[0], { name: 'ducktape_query', args: { operation: 'query', target: { module: 'pages' }, input: { record_collection: { page_id: 'board' } } }, signal });
  assert.equal(calls[1].name, 'ducktape_action');
  assert.equal(calls[1].args.operation, 'submit');
  assert.equal(String(calls[1].args.request_id).length, 64);
  assert.equal(calls[1].signal, signal);
  assert.ok(!JSON.stringify(calls).includes('token'));
});

test('transport admission and mismatched/apparently successful receipts never count as committed', async () => {
  const values = [
    { receipt_id: 'pending', receipt: { target: 'pages', payload: { x: {} }, status: 'awaiting_program' } },
    { receipt: { target: 'pages', payload: { x: {} }, status: { completed: { outcome: { rejected: { reason: 'SECRET' } } } } } },
    { receipt: { target: 'tasks', payload: { x: {} }, status: { completed: { outcome: { applied: {} } } } } },
    { receipt: { target: 'pages', payload: { changed: {} }, status: { completed: { outcome: { applied: {} } } } } },
    // A receipt naming the CALLER's request id instead of the id runs derived
    // proves nothing about which write committed.
    { receipt_id: `action/${digest('test-run')}/${digest(digest('op'))}`,
      receipt: { request_id: digest('op'), operation: 'submit', run_id: 'test-run', target: 'pages', payload: { x: {} }, status: { completed: { outcome: { applied: { assigned: [], output_digest: Array(32).fill(0) } } } } } },
    // …and so does one whose two names disagree about the id it minted.
    { receipt_id: 'action/other/other',
      receipt: { request_id: `action/${digest('test-run')}/${digest(digest('op'))}`, operation: 'submit', run_id: 'test-run', target: 'pages', payload: { x: {} }, status: { completed: { outcome: { applied: { assigned: [], output_digest: Array(32).fill(0) } } } } } },
  ];
  await Promise.all(values.map(value => assert.rejects(createNetworkRpc({ callTool: async () => reply(value) }).submit('pages', { x: {} }, 'op'))));
  await assert.rejects(createNetworkRpc({ callTool: async () => ({ content: [{ type: 'text', text: 'SECRET' }], isError: true }) }).query('pages', {}), /network_tool_refused/);
});

// -- Module fixture: native budgets, protected Pages, projected Files --------
interface DocumentRow { record_id: string; data: { kind: string; value: Record<string, unknown> }; revision: number }
interface StateRow { key: string; value: Record<string, unknown>; revision: number }
interface PageReceipt { page_id: string; request_id: string; revision: number; payload_digest: number[]; metadata: OperationMetadata; artifacts: string[] }
interface PageCommit {
  page_id: string; request_id: string; expected_revision: number; metadata: OperationMetadata; artifacts: string[];
  changes: ({ upsert: { record_id: string; data: DocumentRow['data']; document: unknown } } | { delete: { record_id: string } })[];
  state_changes: ({ put: { key: string; value: Record<string, unknown> } } | { delete: { key: string } })[];
}
interface Job {
  job_id: string; conversation_id: string; kind: string; spec: string; submitter: { account: number }; execution: 'conversation'; created_at_revision: number; attempt: number; status: 'pending' | 'processing' | 'done' | 'failed' | 'cancelled'; previous_job_id: string | null;
  reports: { operation_id: string; attempt: number; kind: string; payload: string }[]; result: { ok: boolean; payload: string } | null;
}
interface NativeActionReceipt {
  request_id: string; account: number; generation: number; run_id: string; operation: string;
  result: unknown; target: string; payload: unknown; status: unknown;
}
interface NativeState {
  revision: number; records: Map<string, DocumentRow>; protected: Map<string, StateRow>; receipts: Map<string, PageReceipt>;
  files: Map<string, Buffer>; snapshots: Map<string, Map<string, Buffer>>; head: string; fileRevision: number;
  jobs: Map<string, Job>; schedules: Map<string, Record<string, unknown>>; actionReceipts: Map<string, NativeActionReceipt>;
  runId: string; turn: number; actions: number; account: unknown; jobAccount: number; loseJobReply: boolean;
  calls: { name: string; args: Record<string, unknown>; runId: string }[];
}
const config: NetworkConfig = { agentId: 'chief', conversationId: 'conversation', boardPageId: 'board', inboxPageId: 'inbox', homePageId: 'home', workerAgentId: 'worker' };
const digest = (value: string): string => createHash('sha256').update(value).digest('hex');
const canonical = (value: unknown): string => {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  const record = value !== null && typeof value === 'object';
  if (!record) return JSON.stringify(value);
  return `{${Object.entries(value as Record<string, unknown>).filter(([, item]) => item !== undefined)
    .sort(([left], [right]) => Buffer.compare(Buffer.from(left), Buffer.from(right)))
    .map(([key, item]) => `${JSON.stringify(key)}:${canonical(item)}`).join(',')}}`;
};
const object = (value: unknown): Record<string, unknown> => {
  assert(value !== null && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
};
const nativeFixture = () => {
  const state: NativeState = { revision: 0, records: new Map(), protected: new Map(), receipts: new Map(), files: new Map(), snapshots: new Map(),
    head: digest('empty'), fileRevision: 0, jobs: new Map(), schedules: new Map(), actionReceipts: new Map(), runId: 'native-turn-0', turn: 0, actions: 0,
    account: 7, jobAccount: 7, loseJobReply: false, calls: [] };
  const nextTurn = (spent = 0): void => {
    assert(spent >= 0 && spent <= 32);
    state.turn++;
    state.runId = `native-turn-${state.turn}`;
    state.actions = spent;
  };
  const snapshot = (paths: Map<string, Buffer>): string => {
    const root = digest(canonical([...paths].sort(([left], [right]) => left.localeCompare(right)).map(([path, content]) => [path, content.toString('base64')])));
    state.snapshots.set(root, new Map(paths));
    return root;
  };
  const publishFiles = (entries: [string, string | Uint8Array][]): string => {
    entries.forEach(([path, content]) => state.files.set(path, typeof content === 'string' ? Buffer.from(content, 'utf8') : Buffer.from(content)));
    state.head = snapshot(state.files);
    return state.head;
  };
  const queryFiles = (input: Record<string, unknown>): unknown => {
    if (input.refs) return { refs: { head: state.head } };
    if (input.stat) return { stat: state.files.has(String(object(input.stat).path)) ? {} : null };
    const read = object(input.read);
    const paths = state.snapshots.get(String(read.snapshot));
    assert(paths, 'Snapshot must exist as a candidate or retained root');
    const content = paths.get(String(read.path));
    assert(content !== undefined, 'Projection must preserve the original absolute Files path');
    const data = Buffer.from(content);
    const offset = Number(read.offset);
    const length = Number(read.len);
    return { read: { b64: data.subarray(offset, offset + length).toString('base64'), eof: offset + length >= data.length } };
  };
  const queryPages = (input: Record<string, unknown>): unknown => {
    if (input.record_collection) return { record_collection: { page_id: 'board', writer: { account: 7 }, revision: state.revision, record_count: state.records.size } };
    if (input.record_state) return { record_state: structuredClone(state.protected.get(String(object(input.record_state).key)) ?? null) };
    if (input.record_receipt) return { record_receipt: structuredClone(state.receipts.get(String(object(input.record_receipt).request_id)) ?? null) };
    const page = object(input.records);
    const rows = [...state.records.values()].sort((left, right) => left.record_id.localeCompare(right.record_id))
      .filter(row => page.after === null || row.record_id > String(page.after));
    assert.equal(page.limit, 32);
    const records = rows.slice(0, 32);
    return { records: { revision: state.revision, records: structuredClone(records), next_after: rows.length > 32 ? records.at(-1)!.record_id : null } };
  };
  const queryRuns = (input: unknown): unknown => {
    if (input === 'agent_sessions') return { agent_sessions: [{ agent_id: config.agentId, run_id: state.runId, actions: state.actions,
      session_key: Array<number>(32).fill(241), lease: { holder: Array<number>(32).fill(242), attempt: 0 }, opened_at: 0 }] };
    const value = object(input);
    if (value.conversation) return { conversation: { conversation_id: config.conversationId, agent_id: config.agentId, account: state.account, source: { channel: { channel_id: 'shared' } } } };
    if (value.conversation_schedules) return { conversation_schedules: [...state.schedules.values()] };
    if (value.action_request) return { action_request: structuredClone(state.actionReceipts.get(String(object(value.action_request).request_id)) ?? null) };
    throw new Error('Unexpected Runs query');
  };
  const queryTasks = (input: Record<string, unknown>): unknown => {
    const job = object(input.job);
    if (job.get) return { job: { job: structuredClone(state.jobs.get(String(object(job.get).job_id)) ?? null) } };
    if (job.get_worker) return { job: { worker: { executions: [...state.jobs.values()].filter(item => item.conversation_id === object(job.get_worker).conversation_id).map(item => structuredClone(item)) } } };
    throw new Error('Unexpected Jobs query');
  };
  const queryModule = (module: string, input: unknown): unknown => {
    switch (module) {
      case 'pages': return queryPages(object(input));
      case 'files': return queryFiles(object(input));
      case 'runs': return queryRuns(input);
      case 'tasks': return queryTasks(object(input));
      default: throw new Error(`Unexpected module ${module}`);
    }
  };
  const applyPages = (input: Record<string, unknown>): number[] => {
    const commit = input.commit_records as PageCommit;
    assert(commit);
    assert.equal(commit.page_id, config.boardPageId);
    assert.equal(commit.expected_revision, state.revision);
    assert(commit.changes.length + commit.state_changes.length <= 16);
    assert(Buffer.byteLength(canonical(input)) <= 128 * 1024);
    assert(commit.artifacts.length <= 8 && commit.artifacts.includes(commit.metadata.history.hash));
    assert(commit.artifacts.every(root => state.snapshots.has(root)), 'Every retained root must already exist');
    const revision = state.revision + 1;
    commit.changes.forEach(change => {
      if ('delete' in change) { state.records.delete(change.delete.record_id); return; }
      assert(['task', 'ask', 'rule'].includes(change.upsert.data.kind));
      state.records.set(change.upsert.record_id, { record_id: change.upsert.record_id, data: structuredClone(change.upsert.data), revision });
    });
    commit.state_changes.forEach(change => {
      if ('delete' in change) { state.protected.delete(change.delete.key); return; }
      state.protected.set(change.put.key, { ...structuredClone(change.put), revision });
    });
    assert(state.records.size <= 1024 && state.protected.size <= 256);
    state.revision = revision;
    state.receipts.set(commit.request_id, { page_id: commit.page_id, request_id: commit.request_id, revision,
      metadata: structuredClone(commit.metadata), artifacts: [...commit.artifacts], payload_digest: [...createHash('sha256').update(canonical(input)).digest()] });
    return [];
  };
  const applyFiles = (input: Record<string, unknown>): number[] => {
    state.fileRevision++;
    const output = (outcome: unknown): number[] => [...Buffer.from(JSON.stringify({ actor: { Account: 7 }, source_revision: state.fileRevision, outcome }))];
    if (input.commit) {
      const commit = input.commit as { changes: { put: { path: string; content: { inline: { b64: string } } } }[] };
      commit.changes.forEach(({ put }) => state.files.set(put.path, Buffer.from(put.content.inline.b64, 'base64')));
      state.head = snapshot(state.files);
      return output({ commit: { snapshot: state.head } });
    }
    const projection = object(input.project_snapshot);
    const path = String(projection.path);
    const content = state.snapshots.get(String(projection.snapshot))?.get(path);
    assert(content !== undefined);
    return output({ project_snapshot: { snapshot: snapshot(new Map([[path, content]])) } });
  };
  const writeJob = (value: Record<string, unknown>, conversationId: string, previous: string | null): number[] => {
    const jobId = String(value.job_id);
    assert(!state.jobs.has(jobId), 'No repeated independent worker dispatch');
    assert.equal(value.kind, `agent/${config.workerAgentId}`, 'Both fresh and continued Jobs explicitly select the worker kind');
    state.jobs.set(jobId, { job_id: jobId, conversation_id: conversationId, kind: String(value.kind), spec: String(value.spec), submitter: { account: state.jobAccount },
      execution: 'conversation', created_at_revision: state.jobs.size + 1, status: 'pending', attempt: 0, previous_job_id: previous, reports: [], result: null });
    return [];
  };
  const applyTasks = (input: Record<string, unknown>): number[] => {
    const job = object(input.job);
    if (job.submit_conversation) {
      const submit = object(job.submit_conversation);
      return writeJob(submit, `worker-${String(submit.job_id)}`, null);
    }
    const continuation = object(job.continue);
    const previous = state.jobs.get(String(continuation.previous_job_id));
    assert(previous);
    assert.equal(previous.execution, 'conversation');
    return writeJob(continuation, previous.conversation_id, previous.job_id);
  };
  const applyRuns = (input: Record<string, unknown>): number[] => {
    const schedule = object(input.schedule_conversation_input);
    state.schedules.set(String(schedule.schedule_id), { ...schedule, status: schedule.after_secs === null ? 'cancelled' : { pending: { due_at: Number(schedule.after_secs) } } });
    return [];
  };
  const applyModule = (module: string, input: Record<string, unknown>): number[] => {
    switch (module) {
      case 'pages': return applyPages(input);
      case 'files': return applyFiles(input);
      case 'tasks': return applyTasks(input);
      case 'runs': return applyRuns(input);
      default: throw new Error(`Unexpected module ${module}`);
    }
  };
  const recordAction = (args: Record<string, unknown>, outcome: unknown) => {
    const receiptId = `action/${digest(state.runId)}/${digest(String(args.request_id))}`;
    // Runs answers with the id it derived, never the caller's own request id.
    const receipt: NativeActionReceipt = { request_id: receiptId, account: 7, generation: 1,
      operation: 'submit', run_id: state.runId, result: null, target: String(object(args.target).module), payload: structuredClone(args.input),
      status: { completed: { call: { requester: 'runs', invocation: state.runId, step: state.actions }, outcome } } };
    state.actionReceipts.set(receiptId, structuredClone(receipt));
    return reply({ receipt_id: receiptId, receipt });
  };
  const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
    signal?.throwIfAborted();
    state.calls.push({ name, args: structuredClone(args), runId: state.runId });
    if (name === 'ducktape_whoami') { assert.deepEqual(args, {}); return reply({ agent_id: config.agentId, run_id: state.runId }); }
    const module = String(object(args.target).module);
    if (name === 'ducktape_query') return reply(queryModule(module, args.input));
    assert.equal(name, 'ducktape_action');
    assert.equal(args.operation, 'submit');
    assert.equal(String(args.request_id).length, 64);
    state.actions++;
    assert(state.actions <= 32, 'Native action budget is fixed at 32 per run');
    const assigned = applyModule(module, object(args.input));
    const result = recordAction(args, { applied: { assigned, output_digest: Array<number>(32).fill(0) } });
    if (module === 'tasks' && state.loseJobReply) { state.loseJobReply = false; throw new Error('lost_job_reply'); }
    return result;
  } };
  const pages = createNetworkPages(createNetworkRpc(adapter), config.boardPageId, config.conversationId);
  const board = (): Promise<Board> => Promise.resolve().then(() => pages.read()).then(value => value.value as Board);
  const rawReports = (): string[] => {
    const histories = new Set([...state.receipts.values()].map(receipt => `/shared/agents/chief/reports/${receipt.metadata.history.fileId}.txt`));
    return [...state.files].filter(([path]) => !histories.has(path)).map(([, content]) => content.toString('utf8'));
  };
  const collectUnretainedSnapshots = (): void => {
    const retained = new Set([...state.receipts.values()].flatMap(receipt => receipt.artifacts));
    [...state.snapshots.keys()].filter(root => root !== state.head && !retained.has(root)).forEach(root => state.snapshots.delete(root));
  };
  return { state, adapter, nextTurn, board, rawReports, publishFiles, collectUnretainedSnapshots, recordAction, start: () => createNetworkService(adapter, config) };
};
const taskCommand = (expectedRevision: number): ChiefCommand => ({ kind: 'change', operationId: 'task', expectedRevision,
  action: { kind: 'task_put', task: { id: 'task1', key: 'task1', title: 'One authorized task', brief: 'Verify concrete results', scope: ['src'], access: 'read', dependencies: [] } } });
const dispatchCommand = (expectedRevision: number): Extract<ChiefCommand, { kind: 'dispatch' }> => ({ kind: 'dispatch', operationId: 'run1', expectedRevision, taskId: 'task1', fresh: true });
const jobEvent = (job: Job, operationId: string): CommittedConversationEvent => ({ sequence: 20, operation_id: operationId, admitted_at: 1, actor: { Module: 'tasks' },
  input: { event: { kind: 'attribution', content: { attribution: { source: { module: 'tasks', kind: 'job', object: job.job_id } }, source: structuredClone(job) } } } });
const immutableJobEvent = (job: Job, operationId: string, operation: Record<string, unknown>, sequence: number): CommittedConversationEvent => {
  const actor = { module: 'runs' };
  return { sequence, operation_id: operationId, admitted_at: 1, actor: { Module: 'tasks' }, input: { event: { kind: 'attribution', content: {
    attribution: { actor, source: { module: 'tasks', kind: 'job_event', object: digest(operationId) } },
    source: { job_id: job.job_id, conversation_id: job.conversation_id, job_kind: job.kind, created_at_revision: job.created_at_revision,
      job_attempt: job.attempt, submitter: job.submitter, actor, height: 1, operation },
  } } } };
};
const checkpointEvent = (job: Job, operationId: string, payload: string, kind: 'checkpoint' | 'report' = 'checkpoint', sequence = 10): CommittedConversationEvent =>
  immutableJobEvent(job, operationId, { checkpoint: { job_id: job.job_id, operation_id: operationId, attempt: job.attempt, kind, payload } }, sequence);
const boardCommand: ChiefCommand = { kind: 'board', section: 'overview', offset: 0, limit: 10 };
const artifactPath = (fileId: string): string => `/shared/agents/chief/reports/${fileId}.txt`;
const claimedFiles = (fixture: ReturnType<typeof nativeFixture>, count: number): FileRef[] => {
  const contents = Array.from({ length: count }, (_, index) => `Claimed artifact ${index}`);
  const ids = contents.map(content => `report-${digest(content)}`);
  const head = fixture.publishFiles([...contents.map((content, index): [string, string] => [artifactPath(ids[index]), content]), ['/private/unrelated.txt', 'Unrelated data must never enter a claim root']]);
  return ids.map(fileId => ({ fileId, hash: head }));
};
const retainedRoots = (fixture: ReturnType<typeof nativeFixture>): Set<string> => new Set([...fixture.state.receipts.values()].flatMap(receipt => receipt.artifacts));
const artifactContent = (fixture: ReturnType<typeof nativeFixture>, ref: FileRef): string => {
  const root = fixture.state.snapshots.get(ref.hash);
  assert(root && root.size === 1);
  const content = root.get(artifactPath(ref.fileId));
  assert(content);
  return content.toString('utf8');
};
const memberEvent = (): CommittedConversationEvent => {
  const actor = { External: Array<number>(32).fill(7) };
  return { sequence: 20, operation_id: 'later-chat', admitted_at: 1, actor, input: { chat: { message: { head: { message_id: 'later-chat-message', deleted: false,
    rev: 0, edited_at: null, origin: actor, content_origin: actor, author: { account: 7 }, blocks: [{ paragraph: [{ text: 'Please review the preserved blocker.' }] }] } } } } };
};

// -- Real package service through the generic catalog ------------------------
test('scoped Files projections and immutable Pages roots preserve retired reports without Pin', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  const dispatch = dispatchCommand(fixture.state.revision);
  assert((await service.execute(dispatch)).success);
  assert.equal(fixture.state.actions, 11);
  assert.equal((await fixture.board()).outbox.length, 0);
  const job = [...fixture.state.jobs.values()][0];
  assert.deepEqual(job.submitter, { account: 7 });
  fixture.nextTurn();
  job.status = 'processing';
  job.reports = [{ operation_id: 'checkpoint', attempt: 0, kind: 'checkpoint', payload: JSON.stringify({ summary: 'Verified the seam', next: 'Review', artifacts: [] }) }];
  assert.equal((await service.prepareInputs([checkpointEvent(job, 'progress', job.reports[0].payload)])).wake, false);
  assert.equal((await fixture.board()).runs[0].progress?.sequence, 10);
  assert.equal(fixture.state.actions, 5);
  const checkpointSource = (await fixture.board()).runs[0].progress!.source!;
  assert(checkpointSource);
  assert(fixture.state.snapshots.get(checkpointSource.hash)!.size === 1);
  assert.deepEqual(fixture.rawReports(), [job.reports[0].payload]);
  fixture.nextTurn();
  const raw = 'RAW_PROVIDER_REPORT_MUST_STAY_IN_FILES';
  job.status = 'done';
  job.result = { ok: true, payload: raw };
  const prepared = await service.prepareInputs([jobEvent(job, 'terminal')]);
  assert(prepared.wake && prepared.prompt && !prepared.prompt.includes(raw));
  assert(fixture.state.actions <= 9);
  assert.deepEqual(fixture.rawReports(), [job.reports[0].payload, raw]);
  const review = await fixture.board();
  assert.equal(review.tasks[0].status, 'review');
  const report = review.runs[0].report!;
  assert.notEqual(report.hash, report.fileId.slice(7), 'A FileRef hashes the projected root, not raw content');
  assert.equal(fixture.state.snapshots.get(report.hash)!.size, 1);
  fixture.nextTurn();
  assert((await service.execute({ kind: 'change', operationId: 'accept', expectedRevision: review.revision,
    action: { kind: 'accept', taskId: 'task1', outcome: 'Independently reviewed the outcome', evidence: [report] } })).success);
  assert.equal((await fixture.board()).runs.length, 0);
  assert.equal(fixture.state.records.size, 1);
  assert.equal(fixture.state.protected.size, 3);
  fixture.collectUnretainedSnapshots();
  fixture.nextTurn(32);
  const from = fixture.state.calls.length;
  assert((await service.execute(boardCommand)).success);
  const historical = await service.execute({ kind: 'report', runId: 'run1' });
  assert(historical.success);
  assert.deepEqual(historical.data.artifact, report);
  const reads = fixture.state.calls.slice(from);
  assert(reads.every(call => call.name === 'ducktape_query'));
  assert(!reads.some(call => object(call.args.target).module === 'runs'), 'Read-only commands do not query budgets');
  fixture.nextTurn();
  assert((await service.execute(dispatch)).success);
  assert.equal(fixture.state.jobs.size, 1);
  assert.equal(fixture.state.actions, 0);
  assert(!fixture.state.calls.some(call => call.args.input !== null && typeof call.args.input === 'object' && Object.hasOwn(call.args.input, 'pin')));
  assert(![...fixture.state.files.values()].some(content => content.includes('session_key')));
  fixture.nextTurn();
  assert((await service.execute({ kind: 'change', operationId: 'reopen', expectedRevision: fixture.state.revision,
    action: { kind: 'task_status', taskId: 'task1', status: 'queued', reason: 'Authorized follow-up in retained worker conversation' } })).success);
  fixture.nextTurn();
  const historyReads: string[] = [];
  const continuing = await createNetworkService({ callTool: async (name, args, signal) => {
    const input = typeof args.input === 'object' ? object(args.input) : undefined;
    const jobInput = input?.job ? object(input.job) : undefined;
    if (name === 'ducktape_query' && jobInput?.get_worker) {
      historyReads.push('read');
      const response = await fixture.adapter.callTool(name, args, signal);
      // A later native execution appears after preparation reads history. The
      // CAS winner must submit the previous job it froze, not resolve again.
      fixture.state.jobs.set('later-execution', { ...structuredClone(job), job_id: 'later-execution' });
      return response;
    }
    if (name === 'ducktape_action' && jobInput?.continue) {
      assert(Object.isFrozen(input) && Object.isFrozen(jobInput) && Object.isFrozen(jobInput.continue));
      assert.equal(object(jobInput.continue).previous_job_id, job.job_id);
      const entry = (await fixture.board()).outbox[0];
      assert.equal(entry.status, 'attempted');
      assert.equal(entry.effect!.payloadFingerprint, digest(canonical(input)));
    }
    return fixture.adapter.callTool(name, args, signal);
  } }, config);
  assert((await continuing.execute({ kind: 'dispatch', operationId: 'run2', expectedRevision: fixture.state.revision, taskId: 'task1', fresh: false })).success);
  assert.equal(historyReads.length, 1);
  const resumed = [...fixture.state.jobs.values()].find(item => item.previous_job_id === job.job_id);
  assert(resumed);
  assert.equal(resumed.conversation_id, job.conversation_id);
  assert.equal(resumed.kind, `agent/${config.workerAgentId}`);
  assert.equal(fixture.state.actions, 11);
});

test('immutable report claims preserve Files provenance but cannot complete work or borrow another actor', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  assert((await service.execute(dispatchCommand(fixture.state.revision))).success);
  const job = [...fixture.state.jobs.values()][0];
  fixture.nextTurn();
  const raw = 'RAW_NONTERMINAL_CLAIM';
  const prepared = await service.prepareInputs([checkpointEvent(job, 'report-claim', raw, 'report')]);
  assert(prepared.wake && prepared.prompt && !prepared.prompt.includes(raw));
  assert.equal(fixture.state.actions, 5);
  const board = await fixture.board();
  assert.equal(board.tasks[0].status, 'running');
  assert.notEqual(board.runs[0].status, 'completed');
  assert(board.runs[0].report);
  assert.deepEqual(fixture.rawReports(), [raw]);
  fixture.nextTurn();
  const forged = checkpointEvent(job, 'forged-actor', JSON.stringify({ summary: 'Forged progress', next: 'Do not follow', artifacts: [] }));
  const content = object(object(object(forged.input).event).content);
  object(content.attribution).actor = { account: 99 };
  const quarantined = await service.prepareInputs([forged]);
  assert(quarantined.wake && quarantined.prompt);
  assert.equal(JSON.parse(quarantined.prompt).events[0].reason, 'wrong_worker_actor');
  assert(!quarantined.prompt.includes('Forged progress'));
  assert.equal(fixture.state.actions, 0);
  assert.deepEqual(await fixture.board(), board);
  fixture.nextTurn();
  const final = 'Final worker result';
  assert((await service.prepareInputs([immutableJobEvent(job, 'finalized', { finalize: { job_id: job.job_id, ok: true, payload: final } }, 11)])).wake);
  assert.equal((await fixture.board()).tasks[0].status, 'review');
  const revision = fixture.state.revision;
  fixture.nextTurn();
  job.status = 'done';
  job.result = { ok: true, payload: final };
  await service.prepareInputs([jobEvent(job, 'generic-result')]);
  assert.equal(fixture.state.revision, revision, 'Generic result and immutable finalize share job-incarnation identity');
  assert.equal(fixture.state.actions, 0);
  assert.deepEqual(fixture.rawReports(), [raw, final]);
});

test('four valid full-HEAD claims become file-only roots and raw checkpoint/history fit nine writes', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  assert((await service.execute(dispatchCommand(fixture.state.revision))).success);
  const job = [...fixture.state.jobs.values()][0];
  const claims = claimedFiles(fixture, 4);
  const raw = JSON.stringify({ summary: 'Scoped artifact claims', next: 'Review the exact files', artifacts: claims });
  fixture.nextTurn();
  assert.equal((await service.prepareInputs([checkpointEvent(job, 'four-claims', raw)])).wake, false);
  assert.equal(fixture.state.actions, 9);
  const progress = (await fixture.board()).runs[0].progress!;
  assert.equal(progress.artifacts.length, 4);
  assert.equal(progress.blocker, undefined);
  assert.equal(artifactContent(fixture, progress.source!), raw);
  const roots = retainedRoots(fixture);
  assert(roots.has(progress.source!.hash));
  assert(!roots.has(claims[0].hash), 'The raw full-HEAD hash is data, never a retention root');
  progress.artifacts.forEach((ref, index) => {
    assert.equal(ref.fileId, claims[index].fileId);
    assert.notEqual(ref.hash, claims[index].hash);
    assert(roots.has(ref.hash));
    assert.equal(artifactContent(fixture, ref), `Claimed artifact ${index}`);
  });
  fixture.collectUnretainedSnapshots();
  assert(!fixture.state.snapshots.has(claims[0].hash));
  assert.equal(artifactContent(fixture, progress.source!), raw);
});

const badClaims: { name: string; claims: (fixture: ReturnType<typeof nativeFixture>) => FileRef[] }[] = [
  { name: 'nonexistent', claims: () => [{ fileId: `report-${digest('missing')}`, hash: digest('missing-root') }] },
  { name: 'unsupported namespace', claims: fixture => [{ fileId: 'outside-chief-scope', hash: fixture.publishFiles([['/private/unsupported', 'Not a Chief artifact']]) }] },
  { name: 'over four', claims: fixture => claimedFiles(fixture, 5) },
  { name: 'non-UTF8 bytes', claims: fixture => {
    const fileId = `report-${digest('\ufffd')}`;
    return [{ fileId, hash: fixture.publishFiles([[artifactPath(fileId), Uint8Array.of(255)]]) }];
  } },
];
badClaims.forEach(({ name, claims }) => test(`${name} artifact claims preserve a blocker, never poison roots or later Chat`, async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  assert((await service.execute(dispatchCommand(fixture.state.revision))).success);
  const job = [...fixture.state.jobs.values()][0];
  const artifacts = claims(fixture);
  const raw = JSON.stringify({ summary: 'Untrusted artifact claims', next: 'Verify them', artifacts });
  fixture.nextTurn();
  const prepared = await service.prepareInputs([checkpointEvent(job, 'bad-artifacts', raw)]);
  assert(prepared.wake && prepared.prompt && !prepared.prompt.includes(raw));
  assert.equal(fixture.state.actions, 5);
  const board = await fixture.board();
  const progress = board.runs[0].progress!;
  assert.equal(progress.blocker, 'unprocessable_worker_artifacts');
  assert.deepEqual(progress.artifacts, []);
  assert.equal(artifactContent(fixture, progress.source!), raw);
  const roots = retainedRoots(fixture);
  assert(roots.has(progress.source!.hash));
  assert(artifacts.every(ref => !roots.has(ref.hash)));
  fixture.collectUnretainedSnapshots();
  assert.equal(artifactContent(fixture, progress.source!), raw);
  fixture.nextTurn();
  const later = await service.prepareInputs([memberEvent()]);
  assert(later.wake && later.prompt?.includes('Please review the preserved blocker.'));
  assert.equal(later.handled, 1);
  assert.equal(fixture.state.actions, 0);
  assert.equal(fixture.state.revision, board.revision);
}));

test('related receipt plus four claims and timer rearm fit the exact thirteen-action worst case', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  fixture.state.loseJobReply = true;
  assert(!(await service.execute(dispatchCommand(fixture.state.revision))).success);
  const job = [...fixture.state.jobs.values()][0];
  const claims = claimedFiles(fixture, 4);
  fixture.state.schedules.get('chief-checkin')!.status = { fired: { sequence: 9 } };
  fixture.nextTurn(19);
  const revision = fixture.state.revision;
  const raw = JSON.stringify({ summary: 'Four scoped claims', next: 'Review', artifacts: claims });
  assert.equal((await service.prepareInputs([checkpointEvent(job, 'worst-progress', raw)])).wake, false);
  assert.equal(fixture.state.actions, 32);
  assert.equal(fixture.state.revision - revision, 2);
  const board = await fixture.board();
  assert.equal(board.outbox.length, 0);
  assert.equal(board.runs[0].progress?.artifacts.length, 4);
  assert.equal(artifactContent(fixture, board.runs[0].progress!.source!), raw);
  assert(!retainedRoots(fixture).has(claims[0].hash));
  assert(fixture.state.schedules.get('chief-checkin')!.status && Object.hasOwn(object(fixture.state.schedules.get('chief-checkin')!.status), 'pending'));
});

test('budget preflight rejects dispatch with ten actions left before any native write', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn(22);
  const from = fixture.state.calls.length;
  const revision = fixture.state.revision;
  const files = fixture.state.files.size;
  assert.deepEqual(await service.execute(dispatchCommand(revision)), { success: false, error: 'native_action_budget_reserved' });
  assert.equal(fixture.state.actions, 22);
  assert.equal(fixture.state.revision, revision);
  assert.equal(fixture.state.files.size, files);
  assert.equal(fixture.state.jobs.size, 0);
  assert.deepEqual(fixture.state.calls.slice(from).map(call => call.name), ['ducktape_whoami', 'ducktape_query']);
  assert.equal(fixture.state.calls.at(-1)!.args.input, 'agent_sessions');
});

test('related receipt, terminal result and timer fit exactly nine remaining actions', async () => {
  const fixture = nativeFixture();
  const service = await fixture.start();
  assert((await service.execute(taskCommand(0))).success);
  fixture.nextTurn();
  fixture.state.loseJobReply = true;
  assert(!(await service.execute(dispatchCommand(fixture.state.revision))).success);
  assert.equal((await fixture.board()).outbox[0].status, 'attempted');
  const job = [...fixture.state.jobs.values()][0];
  job.status = 'done';
  job.result = { ok: true, payload: 'Finished; raw claim remains in Files' };
  job.reports = [{ operation_id: 'checkpoint', attempt: 0, kind: 'checkpoint', payload: JSON.stringify({ summary: 'Done checking', next: 'Review', artifacts: [] }) }];
  fixture.nextTurn(23);
  const before = fixture.state.revision;
  const prepared = await service.prepareInputs([jobEvent(job, 'terminal-with-checkpoint')]);
  assert(prepared.wake);
  assert.equal(fixture.state.actions, 32);
  assert.equal(fixture.state.revision - before, 2, 'One receipt commit plus one terminal commit');
  const board = await fixture.board();
  assert.equal(board.tasks[0].status, 'review');
  assert.equal(board.outbox.length, 0);
  assert.equal(board.runs[0].progress, undefined, 'A terminal snapshot never rereads mutable checkpoint rows');
  assert.equal(fixture.rawReports().length, 1);
  assert.equal(fixture.state.jobs.size, 1);
  assert.equal(fixture.state.schedules.get('chief-checkin')!.status, 'cancelled');
  fixture.nextTurn();
  const from = fixture.state.calls.length;
  await assert.rejects(service.prepareInputs([jobEvent(job, 'one'), jobEvent(job, 'two')]), /resident_event_batch_exceeds_turn/);
  assert.equal(fixture.state.calls.length, from);
});

test('native account identity and action cap are checked against authoritative fixtures', async () => {
  const rust = await readFile(new URL('../../crates/modules/apps/runs/src/interface.rs', import.meta.url), 'utf8');
  assert(rust.includes('pub const MAX_ACTIONS_PER_SESSION: u32 = 32;'));
  assert.equal(MAX_NATIVE_ACTIONS, 32);
  const invalid = nativeFixture();
  invalid.state.account = Number.MAX_SAFE_INTEGER + 1;
  await assert.rejects(invalid.start(), /invalid_chief_account/);
  const wrongOwner = nativeFixture();
  const service = await wrongOwner.start();
  assert((await service.execute(taskCommand(0))).success);
  wrongOwner.nextTurn();
  wrongOwner.state.jobAccount = 8;
  const refused = await service.execute(dispatchCommand(wrongOwner.state.revision));
  assert(!refused.success);
  assert.equal((await wrongOwner.board()).outbox[0].status, 'attempted');
});

test('authoritative non-applied Job receipts block work, retire outbox and never retry', async () => {
  for (const outcome of [{ rejected: { reason: 'SECRET_native_reason' } }, { refused: 'revoked' }, { unrepresentable: { attempted: 'rejected' } }]) {
    const h = nativeFixture();
    const attempts: string[] = [];
    const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
      if (name !== 'ducktape_action' || object(args.target).module !== 'tasks') return h.adapter.callTool(name, args, signal);
      h.state.actions++;
      const requestId = String(args.request_id);
      attempts.push(requestId);
      return h.recordAction(args, outcome);
    } };
    const service = await createNetworkService(adapter, config);
    assert((await service.execute(taskCommand(0))).success);
    h.nextTurn();
    const command = dispatchCommand(h.state.revision);
    const rejected = await service.execute(command);
    assert(rejected.success, JSON.stringify(rejected));
    assert.equal(rejected.data.status, 'rejected');
    const board = await h.board();
    assert.equal(board.tasks[0].status, 'blocked');
    assert.equal(board.runs[0].status, 'failed');
    assert.equal(board.outbox.length, 0);
    assert.equal(h.state.jobs.size, 0);
    const audit = [...h.state.receipts.values()].find(record => (record.metadata.result?.effectReceipt as { kind?: string } | undefined)?.kind === 'rejected');
    assert(audit);
    assert(!JSON.stringify(audit).includes('SECRET_native_reason'));
    h.nextTurn();
    assert((await service.execute(command)).success);
    assert.equal(attempts.length, 1);
    assert.equal(h.state.actions, 0);
  }
});

const effectReceipt = (metadata: OperationMetadata): EffectReceipt | undefined => metadata.result?.effectReceipt as EffectReceipt | undefined;

test('a rejected native action survives an undurable acknowledgement and reconciles from its original run', async () => {
  for (const outcome of [{ rejected: { reason: 'SECRET_native_reason' } }, { refused: 'revoked' },
    { unrepresentable: { attempted: 'rejected' } }, { unrepresentable: { attempted: 'refused' } }]) {
    for (const failure of ['transport', 'cas']) {
      const h = nativeFixture();
      const fault = { pending: true };
      const submissions: Record<string, unknown>[] = [];
      const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
        const action = name === 'ducktape_action';
        const target = action ? object(args.target).module : undefined;
        if (target === 'tasks') {
          // Observe the durable attempt at the actual native send boundary.
          const entry = (await h.board()).outbox[0];
          assert.equal(entry.status, 'attempted');
          assert(entry.attemptId);
          assert.deepEqual(entry.effect, { requestId: args.request_id, runId: h.state.runId, target: 'tasks',
            receiptId: `action/${digest(h.state.runId)}/${digest(String(args.request_id))}`, payloadFingerprint: digest(canonical(args.input)) });
          submissions.push(structuredClone(args));
          h.state.actions++;
          return h.recordAction(args, outcome);
        }
        const commit = target === 'pages' ? object(args.input).commit_records as PageCommit : undefined;
        const rejectionAck = commit && effectReceipt(commit.metadata)?.kind === 'rejected';
        if (rejectionAck && fault.pending) {
          fault.pending = false;
          h.state.actions++;
          // Neither fault lets applyPages mutate documents/protected state.
          if (failure === 'transport') throw new Error('lost_before_ack_durability');
          return h.recordAction(args, { rejected: { reason: 'revision_conflict' } });
        }
        return h.adapter.callTool(name, args, signal);
      } };
      const service = await createNetworkService(adapter, config);
      assert((await service.execute(taskCommand(0))).success);
      h.nextTurn();
      const command = dispatchCommand(h.state.revision);
      assert(!(await service.execute(command)).success);
      const before = await h.board();
      assert.equal(before.outbox.length, 1);
      assert.equal(before.outbox[0].status, 'attempted');
      assert.equal(before.tasks[0].status, 'running');
      assert.equal(before.runs[0].status, 'reserved');
      const identity = before.outbox[0].effect!;
      assert(h.state.actionReceipts.has(identity.receiptId));
      assert(![...h.state.receipts.values()].some(receipt => effectReceipt(receipt.metadata)?.kind === 'rejected'));
      h.nextTurn();
      assert.notEqual(h.state.runId, identity.runId);
      const recreated = await createNetworkService(adapter, config);
      const from = h.state.calls.length;
      const result = await recreated.execute({ kind: 'reconcile', operationId: command.operationId });
      assert(result.success, JSON.stringify(result));
      assert.equal(result.data.status, 'rejected');
      const after = await h.board();
      assert.equal(after.tasks[0].status, 'blocked');
      assert.equal(after.runs[0].status, 'failed');
      assert.deepEqual(after.outbox, []);
      assert.equal(after.revision, before.revision + 1, 'One acknowledgement CAS retires outbox and blocks/fails work atomically');
      const queries = h.state.calls.slice(from).filter(call => call.name === 'ducktape_query' && typeof call.args.input === 'object' && object(call.args.input).action_request);
      assert.equal(queries.length, 1);
      assert.deepEqual(queries[0].args.input, { action_request: { request_id: identity.receiptId } });
      const audit = [...h.state.receipts.values()].find(receipt => effectReceipt(receipt.metadata)?.kind === 'rejected')!;
      assert(audit);
      assert.equal((effectReceipt(audit.metadata) as Extract<EffectReceipt, { kind: 'rejected' }>).receiptId, identity.receiptId);
      assert(!JSON.stringify(audit).includes('SECRET_native_reason'));
      h.nextTurn();
      assert((await recreated.execute(command)).success);
      assert((await recreated.execute({ kind: 'reconcile', operationId: command.operationId })).success);
      assert.equal(h.state.actions, 0);
      assert.equal(submissions.length, 1);
      assert.equal(h.state.jobs.size, 0);
    }
  }
});

test('readonly recovery refuses native fingerprint/locator mismatches and unknown outcomes without another submit', async () => {
  for (const fault of ['payload', 'request', 'run', 'operation', 'target', 'missing', 'transport', 'pending', 'applied_unknown']) {
    const h = nativeFixture();
    const attempts: string[] = [];
    const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
      const action = name === 'ducktape_action' && object(args.target).module === 'tasks';
      if (action) {
        h.state.actions++;
        attempts.push(String(args.request_id));
        h.recordAction(args, { rejected: { reason: 'private' } });
        throw new Error('lost_native_reply');
      }
      const receiptQuery = name === 'ducktape_query' && typeof args.input === 'object' && object(args.input).action_request;
      if (receiptQuery && fault === 'transport') throw new Error('query_unavailable');
      return h.adapter.callTool(name, args, signal);
    } };
    const service = await createNetworkService(adapter, config);
    assert((await service.execute(taskCommand(0))).success);
    h.nextTurn();
    const command = dispatchCommand(h.state.revision);
    assert(!(await service.execute(command)).success);
    const before = await h.board();
    const identity = before.outbox[0].effect!;
    const receipt = h.state.actionReceipts.get(identity.receiptId)!;
    switch (fault) {
      case 'payload': receipt.payload = { wrong: {} }; break;
      case 'request': receipt.request_id = 'other-request'; break;
      case 'run': receipt.run_id = 'other-native-run'; break;
      case 'operation': receipt.operation = 'query'; break;
      case 'target': receipt.target = 'pages'; break;
      case 'missing': h.state.actionReceipts.delete(identity.receiptId); break;
      case 'pending': receipt.status = 'awaiting_program'; break;
      case 'applied_unknown': receipt.status = { completed: { outcome: { unrepresentable: { attempted: 'applied' } } } }; break;
      case 'transport': break;
    }
    h.nextTurn();
    const recreated = await createNetworkService(adapter, config);
    const result = await recreated.execute({ kind: 'reconcile', operationId: command.operationId });
    assert(!result.success || result.data.status === 'uncertain', JSON.stringify(result));
    assert.deepEqual(await h.board(), before);
    assert.equal(h.state.actions, 0);
    await recreated.execute(command);
    assert.deepEqual(await h.board(), before);
    assert.equal(attempts.length, 1);
  }
});

test('mismatched rejection identity and unrepresentable applied outcomes stay uncertain', async () => {
  for (const fault of ['request', 'target', 'payload', 'receipt', 'run', 'applied_unknown']) {
    const h = nativeFixture();
    const adapter: NetworkToolAdapter = { callTool: async (name, args, signal) => {
      if (name !== 'ducktape_action' || object(args.target).module !== 'tasks') return h.adapter.callTool(name, args, signal);
      h.state.actions++;
      const requestId = String(args.request_id);
      const outcome = fault === 'applied_unknown' ? { unrepresentable: { attempted: 'applied' } } : { rejected: { reason: 'private' } };
      return reply({ receipt_id: fault === 'receipt' ? 'wrong' : `action/${digest(h.state.runId)}/${digest(requestId)}`, receipt: {
        request_id: fault === 'request' ? 'wrong' : requestId, operation: 'submit', run_id: fault === 'run' ? 'wrong' : h.state.runId,
        target: fault === 'target' ? 'pages' : 'tasks', payload: fault === 'payload' ? {} : args.input, status: { completed: { outcome } },
      } });
    } };
    const service = await createNetworkService(adapter, config);
    assert((await service.execute(taskCommand(0))).success);
    h.nextTurn();
    const result = await service.execute(dispatchCommand(h.state.revision));
    assert(!result.success);
    const board = await h.board();
    assert.equal(board.outbox[0].status, 'attempted');
    assert.equal(board.runs[0].status, 'reserved');
    assert.equal(board.tasks[0].status, 'running');
  }
});
