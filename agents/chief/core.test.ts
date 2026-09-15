// Pure policy and network-failure regressions use committed in-memory adapters;
// no sleeps, worker subprocesses, model credentials or local board files.
import assert from 'node:assert/strict';
import test from 'node:test';

import type { ChiefService, DomainAction, Progress } from './contracts.ts';
import { decide, emptyBoard, validateBoard } from './domain.ts';
import { createChiefService } from './service.ts';
import { createStore } from './store.ts';
import { boardView, VIEW_CHARACTERS } from './views.ts';
import { conversationId, memory, ref, task } from './test-support.ts';

const command = async (service: ChiefService, state: ReturnType<typeof memory>, operationId: string, action: DomainAction) => {
  const result = await service.execute({ kind: 'change', operationId, expectedRevision: state.state.board.revision, action });
  assert.equal(result.success, true, JSON.stringify(result));
  return result;
};
const started = async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  await command(service, adapters, 'put', { kind: 'task_put', task: task() });
  const dispatched = await service.execute({ kind: 'dispatch', operationId: 'run-1', expectedRevision: adapters.state.board.revision, taskId: 'task-1', fresh: false });
  assert.equal(dispatched.success, true, JSON.stringify(dispatched));
  return { adapters, service };
};

test('dispatch operation identity is the durable run and reserved payload identity', async () => {
  const { adapters } = await started();
  const run = adapters.state.board.runs[0];
  assert.equal(run.id, 'run-1');
  assert.equal(run.operationId, run.id);
  const submission = adapters.submissions[0];
  assert.equal(submission.operationId, run.id);
  assert.equal(submission.payload.kind, 'dispatch');
  assert(submission.payload.kind === 'dispatch');
  assert.equal(submission.payload.runId, run.id);
  assert.throws(() => validateBoard({ ...adapters.state.board, runs: [{ ...run, operationId: 'different-operation' }] }, conversationId));
});

test('canonical keys are unique and dependencies reject cycles', () => {
  const board = decide(emptyBoard(conversationId), { kind: 'task_put', task: { ...task(), key: '  One   TASK ' } });
  assert.equal(board.tasks[0].key, 'one task');
  assert.throws(() => decide(board, { kind: 'task_put', task: { ...task('other'), key: 'ONE TASK' } }), /canonical_key_exists/);
  const two = decide(board, { kind: 'task_put', task: task('other', ['task-1']) });
  const cycle = decide(two, { kind: 'task_put', task: { ...task(), dependencies: ['other'] } });
  assert.throws(() => validateBoard(cycle, conversationId), /dependency_cycle/);
});

test('Pages CAS rejects concurrent writers, binds operation bytes, and confirms duplicate committed writes', async () => {
  const adapters = memory();
  const store = createStore(adapters.pages, conversationId, adapters.files);
  const write = (operationId: string) => store.change({ operationId, expectedRevision: 0, intent: { operationId }, transform: board => decide(board, { kind: 'task_put', task: task(operationId) }) });
  const results = await Promise.allSettled([write('a'), write('b')]);
  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1);
  const winner = adapters.state.board.tasks[0].id;
  const replay = await write(winner);
  assert.equal(replay.replayed, true);
  await assert.rejects(store.change({ operationId: winner, expectedRevision: 0, intent: 'changed', transform: board => board }), /operation_id_reused/);
});

test('failed board commit never claims saved and never leaks network errors', async () => {
  const adapters = memory();
  adapters.state.failCommit = true;
  const result = await createChiefService(adapters, conversationId).execute({ kind: 'change', operationId: 'put', expectedRevision: 0, action: { kind: 'task_put', task: task() } });
  assert.deepEqual(result, { success: false, error: 'unconfirmed_network_operation' });
  assert.equal(adapters.state.board.revision, 0);
});

test('lost reservation receipt can acquire exactly one first attempt after restart', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  await command(service, adapters, 'put', { kind: 'task_put', task: task() });
  adapters.state.loseCommit = true;
  const dispatch = { kind: 'dispatch' as const, operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false };
  assert.equal((await service.execute(dispatch)).success, false);
  assert.equal(adapters.submissions.length, 0);
  assert.equal(adapters.state.board.outbox[0].status, 'reserved');
  const restarted = createChiefService(adapters, conversationId);
  const retry = await restarted.execute(dispatch);
  assert.equal(retry.success, true);
  assert.equal(adapters.submissions.length, 1);
  assert.equal(adapters.state.board.runs[0].status, 'queued');
  assert.equal((await restarted.execute(dispatch)).success, true);
  assert.equal(adapters.submissions.length, 1);
  assert.equal((await restarted.execute({ ...dispatch, operationId: 'run-copy', expectedRevision: adapters.state.board.revision, fresh: true })).success, false);
});

test('lost effect receipt recovers after restart without resubmission or duplicate acknowledgment', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  await command(service, adapters, 'put', { kind: 'task_put', task: task() });
  adapters.state.loseEffect = true;
  const dispatch = { kind: 'dispatch' as const, operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false };
  assert.equal((await service.execute(dispatch)).success, false);
  assert.equal(adapters.state.board.outbox[0].status, 'attempted');
  const restarted = createChiefService(adapters, conversationId);
  assert.equal((await restarted.execute({ kind: 'reconcile', operationId: 'run-1' })).success, true);
  const revision = adapters.state.board.revision;
  assert.equal((await restarted.execute(dispatch)).success, true);
  assert.equal(adapters.state.board.revision, revision);
  assert.equal(adapters.submissions.length, 1);
  assert.equal(adapters.state.board.runs[0].jobId, 'job:run-1');
});

test('receipt absence stays uncertain and keeps the reservation', async () => {
  const { adapters, service } = await started();
  await command(service, adapters, 'put2', { kind: 'task_put', task: task('task-2') });
  adapters.state.failEffect = true;
  await service.execute({ kind: 'dispatch', operationId: 'run-2', expectedRevision: adapters.state.board.revision, taskId: 'task-2', fresh: false });
  const before = adapters.submissions.length;
  const result = await service.execute({ kind: 'reconcile', operationId: 'run-2' });
  assert.equal(result.success && result.data.status, 'uncertain');
  assert.equal(adapters.submissions.length, before);
  assert.equal(adapters.state.board.runs[1].status, 'reserved');
});

test('routine progress persists silently; duplicate heartbeat advances only ordering; blocker wakes once', async () => {
  const { adapters, service } = await started();
  const progress: Progress = { sequence: 1, summary: 'Implemented reader', next: 'Test it', artifacts: [] };
  const event = { kind: 'progress' as const, operationId: 'progress1', runId: 'run-1', jobId: 'job:run-1', progress, raw: JSON.stringify(progress) };
  assert.equal((await service.receive(event)).success, true);
  assert.equal(adapters.submissions.length, 1);
  await service.receive({ ...event, operationId: 'progress2', progress: { ...progress, sequence: 2 } });
  assert.equal(adapters.state.board.runs[0].progress?.sequence, 1);
  assert.equal(adapters.state.board.runs[0].observedSequence, 2);
  const blocked = { ...progress, sequence: 3, blocker: 'Need authorization' };
  const blocker = { ...event, operationId: 'blocker', progress: blocked, raw: JSON.stringify(blocked) };
  await service.receive(blocker);
  await createChiefService(adapters, conversationId).receive(blocker);
  assert.equal(adapters.submissions.filter(item => item.payload.kind === 'wake').length, 1);
  const wrong = await service.receive({ ...event, operationId: 'forged', jobId: 'other-job' });
  assert.equal(wrong.success, false);
});

test('report is immutable Files data and review only, explicit acceptance requires fresh evidence', async () => {
  const { adapters, service } = await started();
  assert.equal((await service.execute({ kind: 'change', operationId: 'early', expectedRevision: adapters.state.board.revision, action: { kind: 'accept', taskId: 'task-1', outcome: 'done', evidence: [ref] } })).success, false);
  const event = { kind: 'result' as const, operationId: 'result', runId: 'run-1', jobId: 'job:run-1', status: 'completed' as const, report: 'RAW_PRIVATE_REPORT' };
  assert.equal((await service.receive(event)).success, true);
  assert.equal(adapters.state.board.tasks[0].status, 'review');
  assert.equal(adapters.fileWrites.get('result'), event.report);
  assert.ok(!JSON.stringify(adapters.state.board).includes(event.report));
  const view = await service.execute({ kind: 'report', runId: 'run-1' });
  assert.equal(view.success && view.data.text, event.report);
  assert.equal((await service.receive(event)).success, true);
  assert.equal(adapters.submissions.filter(item => item.payload.kind === 'wake').length, 1);
  await command(service, adapters, 'accept', { kind: 'accept', taskId: 'task-1', outcome: 'Reviewed implementation and tests', evidence: [ref] });
  assert.equal(adapters.state.board.tasks[0].status, 'done');
  await command(service, adapters, 'reopen', { kind: 'task_status', taskId: 'task-1', status: 'queued', reason: 'New authorized follow-up' });
  assert.equal(adapters.state.board.tasks[0].acceptance, undefined);
  assert.deepEqual(adapters.state.board.tasks[0].evidence, [ref]);
});

test('task followup reuses retained conversation; explicit fresh starts distinct history', async () => {
  const { adapters, service } = await started();
  await service.receive({ kind: 'result', operationId: 'result', runId: 'run-1', jobId: 'job:run-1', status: 'completed', report: 'done' });
  await command(service, adapters, 'reopen', { kind: 'task_status', taskId: 'task-1', status: 'queued', reason: 'Follow up' });
  assert.equal((await service.execute({ kind: 'dispatch', operationId: 'run-2', expectedRevision: adapters.state.board.revision, taskId: 'task-1', fresh: false })).success, true);
  const submitted = adapters.submissions.find(item => item.operationId === 'run-2')!;
  assert.equal(submitted.payload.kind, 'dispatch');
  assert.deepEqual(submitted.payload.kind === 'dispatch' && submitted.payload.conversation, { kind: 'continue', conversationId: 'conversation:run-1' });
});

test('live steering changes canonical brief only after applied Jobs acknowledgment', async () => {
  const { adapters, service } = await started();
  adapters.state.holdControl = true;
  const before = adapters.state.board.tasks[0].brief;
  assert.equal((await service.execute({ kind: 'control', operationId: 'steer', expectedRevision: adapters.state.board.revision, runId: 'run-1', control: 'steer', text: 'Replacement authorized brief' })).success, false);
  assert.equal(adapters.state.board.tasks[0].brief, before);
  adapters.receipts.set('steer', { kind: 'control', operationId: 'steer', jobId: 'job:run-1', status: 'applied' });
  await service.execute({ kind: 'reconcile', operationId: 'steer' });
  assert.equal(adapters.state.board.tasks[0].brief, 'Replacement authorized brief');
});

test('member provenance is host-only, one reply removes open ask without resolving it', async () => {
  const { adapters, service } = await started();
  await command(service, adapters, 'ask', { kind: 'ask_open', ask: { id: 'ask-1', key: 'money', title: 'Budget', question: 'Approve spend?', whyMember: 'Spends money', ifUnasked: 'Cannot undo charge', recommendation: 'Wait', options: [{ label: 'Approve', consequence: 'Charge money' }], artifacts: [], blocks: ['task-1'], addressedTo: ['member-1'], sources: [{ taskId: 'task-1', runId: 'run-1', conversationId: 'conversation:run-1' }] } });
  const input = { kind: 'decision' as const, operationId: 'decision', askId: 'ask-1', source: { kind: 'chat' as const, memberId: 'attacker', conversationId, messageId: 'message-1' }, text: 'Approve' };
  assert.equal((await service.receive(input)).success, false);
  const approved = { ...input, source: { ...input.source, memberId: 'member-1' } };
  assert.equal((await service.receive(approved)).success, true);
  assert.equal(adapters.state.board.asks[0].status, 'replied');
  assert.equal((await service.receive(approved)).success, true);
  assert.equal((await service.receive({ ...approved, operationId: 'duplicate' })).success, false);
  await command(service, adapters, 'resolve', { kind: 'ask_resolve', askId: 'ask-1', status: 'answered', resolution: 'Applied member approval to task' });
});

test('bounded views are pull-only, paginated prefixes and oversized detail chunks remain recoverable', () => {
  const board = { ...emptyBoard(conversationId), tasks: Array.from({ length: 8 }, (_, index) => ({ ...task(`task-${index}`), brief: 'x'.repeat(12000), status: 'queued' as const, evidence: [] })) };
  const view = boardView(board, { kind: 'board', section: 'tasks', offset: 0, limit: 25 });
  assert.ok(JSON.stringify(view).length <= VIEW_CHARACTERS);
  assert.equal(view.nextOffset, null);
  const detail = boardView(board, { kind: 'board', section: 'tasks', id: 'task-0', offset: 0, limit: 1 });
  assert.equal(detail.nextDetailOffset, 5000);
  assert.equal(String(detail.detail).length, 5000);
  assert.equal(boardView(board, { kind: 'board', section: 'tasks', offset: 50, limit: 1 }).omitted, 0);
});
