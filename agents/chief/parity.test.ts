// Resident policy retains the pinned Chief's checkpoint, cadence, explicit
// recovery and canonical merge semantics without restoring any local UI/store.
import assert from 'node:assert/strict';
import test from 'node:test';

import { createChiefService } from './service.ts';
import { conversationId, memory, task } from './test-support.ts';

const fixture = async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  await service.execute({ kind: 'change', operationId: 'put', expectedRevision: 0, action: { kind: 'task_put', task: task() } });
  return { adapters, service };
};

test('checkpoint/current actions and checkin cadence persist, are pull-only and validated', async () => {
  const { adapters, service } = await fixture();
  assert.equal(adapters.state.board.checkinMinutes, 10);
  assert.equal((await service.execute({ kind: 'change', operationId: 'checkpoint', expectedRevision: 1, action: { kind: 'checkpoint', focus: 'Finish the approved change', nextActions: ['Review worker evidence', 'Ask before spending'] } })).success, true);
  assert.equal((await service.execute({ kind: 'change', operationId: 'checkin', expectedRevision: 2, action: { kind: 'checkin', minutes: null } })).success, true);
  assert.equal((await service.execute({ kind: 'change', operationId: 'badcheckin', expectedRevision: 3, action: { kind: 'checkin', minutes: 0 } })).success, false);
  const restarted = createChiefService(adapters, conversationId);
  const view = await restarted.execute({ kind: 'board', section: 'overview', offset: 0, limit: 5, query: 'approved' });
  assert.equal(view.success, true);
  assert.equal(adapters.state.board.checkpoint.nextActions.length, 2);
  assert.equal(adapters.state.board.checkinMinutes, null);
  assert.equal(adapters.submissions.length, 0);
});

test('authoritative missing worker is visible interrupted/blocked and never automatically redispatched', async () => {
  const { adapters, service } = await fixture();
  await service.execute({ kind: 'dispatch', operationId: 'run-1', expectedRevision: 1, taskId: 'task-1', fresh: false });
  const event = { kind: 'unavailable' as const, operationId: 'missing', runId: 'run-1', jobId: 'job:run-1' };
  assert.equal((await service.receive(event)).success, true);
  assert.equal(adapters.state.board.runs[0].status, 'interrupted');
  assert.match(adapters.state.board.runs[0].reason!, /worker_unavailable/);
  assert.equal(adapters.state.board.tasks[0].status, 'blocked');
  assert.equal((await createChiefService(adapters, conversationId).receive(event)).success, true);
  assert.equal(adapters.submissions.length, 1);
});

test('merging canonical tasks retains source tombstone and rewires dependents', async () => {
  const { adapters, service } = await fixture();
  await service.execute({ kind: 'change', operationId: 'target', expectedRevision: 1, action: { kind: 'task_put', task: task('target') } });
  await service.execute({ kind: 'change', operationId: 'dependent', expectedRevision: 2, action: { kind: 'task_put', task: task('dependent', ['task-1']) } });
  assert.equal((await service.execute({ kind: 'change', operationId: 'merge', expectedRevision: 3, action: { kind: 'task_merge', sourceId: 'task-1', targetId: 'target' } })).success, true);
  assert.equal(adapters.state.board.tasks.find(item => item.id === 'task-1')?.mergedInto, 'target');
  assert.deepEqual(adapters.state.board.tasks.find(item => item.id === 'dependent')?.dependencies, ['target']);
});
