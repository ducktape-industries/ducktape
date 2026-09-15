// Same-ID concurrent callers are stricter than ordinary optimistic conflicts:
// identical receipts do not identify which process owns an external effect.
import assert from 'node:assert/strict';
import test from 'node:test';

import { createChiefService } from './service.ts';
import { conversationId, memory, task } from './test-support.ts';

test('concurrent identical dispatch requests submit exactly once', async () => {
  const adapters = memory();
  const a = createChiefService(adapters, conversationId);
  const b = createChiefService(adapters, conversationId);
  await a.execute({ kind: 'change', operationId: 'put', expectedRevision: 0, action: { kind: 'task_put', task: task() } });
  const command = { kind: 'dispatch' as const, operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false };
  const results = await Promise.all([a.execute(command), b.execute(command)]);
  assert.ok(results.some(result => result.success));
  assert.equal(adapters.submissions.length, 1);
  assert.equal(adapters.state.board.runs.length, 1);
  assert.equal(adapters.state.board.runs[0].id, command.operationId);
  assert.equal(adapters.state.board.runs[0].operationId, command.operationId);
  assert.equal(adapters.submissions[0].operationId, command.operationId);
});

test('concurrent duplicate result events create one wake after one Files report', async () => {
  const adapters = memory();
  const a = createChiefService(adapters, conversationId);
  const b = createChiefService(adapters, conversationId);
  await a.execute({ kind: 'change', operationId: 'put', expectedRevision: 0, action: { kind: 'task_put', task: task() } });
  await a.execute({ kind: 'dispatch', operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false });
  const event = { kind: 'result' as const, operationId: 'result', runId: 'run-1', jobId: 'job:run-1', status: 'completed' as const, report: 'Report' };
  const results = await Promise.all([a.receive(event), b.receive(event)]);
  assert.ok(results.some(result => result.success));
  assert.equal(adapters.submissions.filter(item => item.payload.kind === 'wake').length, 1);
  assert.equal([...adapters.fileWrites.keys()].filter(id => id === 'result').length, 1);
});
