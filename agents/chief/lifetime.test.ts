// Receipts and pinned artifact roots carry history without consuming work-page
// slots. Removing current operational state must not erase audit or reports.
import assert from 'node:assert/strict';
import test from 'node:test';

import { createChiefService } from './service.ts';
import { conversationId, memory, ref, task } from './test-support.ts';

test('operation retries remain idempotent without a Board.operations collection', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  const commands = Array.from({ length: 40 }, (_, index) => ({ kind: 'change' as const, operationId: `focus-${index}`, expectedRevision: index, action: { kind: 'checkpoint' as const, focus: `Focus ${index}`, nextActions: [] } }));
  await commands.reduce((previous, command) => previous.then(() => service.execute(command)).then(result => assert.equal(result.success, true)), Promise.resolve());
  assert.equal(Object.hasOwn(adapters.state.board, 'operations'), false);
  assert.equal(adapters.state.board.tasks.length, 0);
  assert.equal(adapters.state.board.outbox.length, 0);
  assert.equal((await createChiefService(adapters, conversationId).execute(commands[0])).success, true);
  assert.equal(adapters.state.board.revision, 40);
  assert.equal(adapters.retained.size, 40);
  const receipt = await adapters.pages.receipt('focus-0');
  assert.ok(receipt);
  assert.ok(adapters.retained.has(receipt.history.hash));
});

test('accepted run leaves current state but its raw report remains reachable after restart', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  await service.execute({ kind: 'change', operationId: 'task', expectedRevision: 0, action: { kind: 'task_put', task: task() } });
  const dispatch = { kind: 'dispatch' as const, operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false };
  assert.equal((await service.execute(dispatch)).success, true);
  assert.equal(adapters.state.board.outbox.length, 0);
  await service.receive({ kind: 'result', operationId: 'result', runId: 'run-1', jobId: 'job:run-1', status: 'completed', report: 'Raw immutable report' });
  const report = adapters.state.board.runs[0].report!;
  assert.equal((await service.execute({ kind: 'change', operationId: 'accept', expectedRevision: adapters.state.board.revision, action: { kind: 'accept', taskId: 'task-1', outcome: 'Reviewed accepted outcome', evidence: [ref] } })).success, true);
  assert.equal(adapters.state.board.runs.length, 0);
  assert.equal(adapters.state.board.tasks[0].acceptance?.runId, 'run-1');
  const restarted = createChiefService(adapters, conversationId);
  const pulled = await restarted.execute({ kind: 'report', runId: 'run-1' });
  assert.equal(pulled.success, true, JSON.stringify(pulled));
  assert.deepEqual(pulled.success && pulled.data.artifact, report);
  assert.ok(adapters.retained.has(report.hash));
  assert.equal((await restarted.execute(dispatch)).success, true);
  assert.equal(adapters.submissions.filter(submission => submission.payload.kind === 'dispatch').length, 1);
});

test('uncommitted history artifacts never become retained authoritative state', async () => {
  const adapters = memory();
  adapters.state.failCommit = true;
  const result = await createChiefService(adapters, conversationId).execute({ kind: 'change', operationId: 'focus', expectedRevision: 0, action: { kind: 'checkpoint', focus: 'Not committed', nextActions: [] } });
  assert.equal(result.success, false);
  assert.equal(adapters.state.board.history, null);
  assert.equal(adapters.retained.size, 0);
  assert.equal(await adapters.pages.receipt('focus'), null);
  assert.equal(adapters.fileWrites.size, 1);
});

test('pending control retains a settled run through acceptance until its late receipt is resolved', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  assert((await service.execute({ kind: 'change', operationId: 'task', expectedRevision: 0, action: { kind: 'task_put', task: task() } })).success);
  assert((await service.execute({ kind: 'dispatch', operationId: 'run', expectedRevision: 1, taskId: 'task-1', fresh: true })).success);
  adapters.state.loseEffect = true;
  const control = await service.execute({ kind: 'control', operationId: 'steer', expectedRevision: adapters.state.board.revision, runId: 'run', control: 'steer', text: 'Late new scope' });
  assert(!control.success);
  assert((await service.receive({ kind: 'result', operationId: 'result', runId: 'run', jobId: 'job:run', status: 'completed', report: 'Reviewable result', inboxOperationId: 'native-result' })).success);
  const report = adapters.state.board.runs[0].report!;
  assert((await service.execute({ kind: 'change', operationId: 'accept', expectedRevision: adapters.state.board.revision,
    action: { kind: 'accept', taskId: 'task-1', outcome: 'Reviewed original scope', evidence: [report] } })).success);
  assert.equal(adapters.state.board.tasks[0].status, 'done');
  assert.equal(adapters.state.board.runs.length, 1);
  assert.equal(adapters.state.board.outbox.length, 1);
  assert((await service.execute({ kind: 'reconcile', operationId: 'steer' })).success);
  assert.equal(adapters.state.board.runs.length, 0);
  assert.equal(adapters.state.board.outbox.length, 0);
  assert.equal(adapters.state.board.tasks[0].brief, task().brief);
  const historical = await service.execute({ kind: 'report', runId: 'run' });
  assert(historical.success);
  assert.equal(historical.data.text, 'Reviewable result');
});

test('archived run identity cannot be disguised by a later dispatch', async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  assert((await service.execute({ kind: 'change', operationId: 'task-original', expectedRevision: 0, action: { kind: 'task_put', task: task() } })).success);
  assert((await service.execute({ kind: 'dispatch', operationId: 'dispatch-original', expectedRevision: 1, taskId: 'task-1', fresh: true })).success);
  const originalRunId = adapters.state.board.runs[0].id;
  assert.equal(originalRunId, 'dispatch-original');
  assert((await service.receive({ kind: 'result', operationId: 'result-original', runId: originalRunId, jobId: `job:${originalRunId}`,
    status: 'completed', report: 'Original accepted evidence', inboxOperationId: 'native-result' })).success);
  const evidence = adapters.state.board.runs[0].report!;
  assert((await service.execute({ kind: 'change', operationId: 'accept-original', expectedRevision: adapters.state.board.revision,
    action: { kind: 'accept', taskId: 'task-1', outcome: 'Reviewed original work', evidence: [evidence] } })).success);
  assert((await service.execute({ kind: 'change', operationId: 'task-other', expectedRevision: adapters.state.board.revision,
    action: { kind: 'task_put', task: task('task-2') } })).success);
  assert((await service.execute({ kind: 'dispatch', operationId: 'dispatch-other', expectedRevision: adapters.state.board.revision,
    taskId: 'task-2', fresh: true })).success);
  const otherRunId = adapters.state.board.runs[0].id;
  assert.equal(otherRunId, 'dispatch-other');
  assert((await service.receive({ kind: 'result', operationId: 'result-other', runId: otherRunId, jobId: `job:${otherRunId}`,
    status: 'completed', report: 'Unrelated work', inboxOperationId: 'native-other' })).success);
  const original = await service.execute({ kind: 'report', runId: originalRunId });
  assert(original.success, JSON.stringify(original));
  assert.equal(original.data.taskId, 'task-1');
  assert.equal(original.data.text, 'Original accepted evidence');
  const before = adapters.fileWrites.size;
  const reused = await service.execute({ kind: 'dispatch', operationId: originalRunId, expectedRevision: adapters.state.board.revision, taskId: 'task-2', fresh: true });
  assert.deepEqual(reused, { success: false, error: 'operation_id_reused' });
  assert.equal(adapters.fileWrites.size, before);
  assert.equal(adapters.submissions.length, 2);
});
