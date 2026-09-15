// Native sessions retain action counts across execution retries. Read those
// authoritative counters; neither a process-local reset nor a raised cap is safe.
import assert from 'node:assert/strict';
import test from 'node:test';

import { commandWriteBudget, createNetworkRpc, MAX_NATIVE_ACTIONS } from './network.ts';
import type { NetworkToolAdapter } from './network.ts';

const fixture = (used: () => number) => {
  const calls: string[] = [];
  const adapter: NetworkToolAdapter = { callTool: async (name, args) => {
    calls.push(name);
    if (name === 'ducktape_whoami') return { content: [{ type: 'text', text: JSON.stringify({ agent_id: 'chief', run_id: 'native-run' }) }] };
    assert.equal(name, 'ducktape_query');
    assert.deepEqual(args, { operation: 'query', target: { module: 'runs' }, input: 'agent_sessions' });
    return { content: [{ type: 'text', text: JSON.stringify({ agent_sessions: [
      { run_id: 'other', agent_id: 'worker', actions: 32, session_key: 'must_not_escape' },
      { run_id: 'native-run', agent_id: 'chief', actions: used(), session_key: 'must_not_escape' },
    ] }) }] };
  } };
  return { adapter, calls };
};

test('exact native budget boundary admits a complete plan or rejects before any action', async () => {
  const state = { used: 21 };
  const { adapter, calls } = fixture(() => state.used);
  const rpc = createNetworkRpc(adapter);
  assert.equal(MAX_NATIVE_ACTIONS, 32);
  assert.equal(await rpc.requireBudget('chief', 11), undefined);
  state.used = 22;
  await assert.rejects(rpc.requireBudget('chief', 11), /native_action_budget_reserved/);
  assert.ok(calls.every(name => name !== 'ducktape_action'));
  assert.equal(await rpc.requireBudget('chief', 10), undefined);
});

test('a fresh package instance cannot reset native actions spent by an earlier attempt', async () => {
  const { adapter } = fixture(() => 30);
  await assert.rejects(createNetworkRpc(adapter).requireBudget('chief', 4), /native_action_budget_reserved/);
  await assert.rejects(createNetworkRpc(adapter).requireBudget('chief', 4), /native_action_budget_reserved/);
  assert.equal(await createNetworkRpc(adapter).requireBudget('chief', 2), undefined);
});

test('read-only pulls remain free and a worst-case prepared event leaves room for useful coordination', () => {
  assert.equal(commandWriteBudget({ kind: 'board', section: 'overview', offset: 0, limit: 1 }), 0);
  assert.equal(commandWriteBudget({ kind: 'report', runId: 'run' }), 0);
  const dispatch = commandWriteBudget({ kind: 'dispatch', operationId: 'run', expectedRevision: 0, taskId: 'task', fresh: false });
  const checkpoint = commandWriteBudget({ kind: 'change', operationId: 'checkpoint', expectedRevision: 0, action: { kind: 'checkpoint', focus: '', nextActions: [] } });
  assert.equal(dispatch, 11);
  assert.equal(checkpoint, 4);
  assert.ok(9 + dispatch + checkpoint + checkpoint <= MAX_NATIVE_ACTIONS);
});
