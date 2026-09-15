// Exercise the actual ducktape mcp stdio binary against an event-driven loopback
// node/action fixture. This verifies the real catalog envelope/auth pipe; module
// consensus behavior is covered by Rust module tests and network-pages.test.ts.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { createInterface } from 'node:readline';

import { createNetworkRpc } from './network.ts';
import type { NetworkToolAdapter } from './network.ts';

const binary = process.argv[2];
assert.ok(binary, 'Pass the checkout-built ducktape binary as argv[2]');
const seen: { path: string; body: unknown }[] = [];
const server = createServer((request, response) => {
  const chunks: Buffer[] = [];
  request.on('data', chunk => chunks.push(Buffer.from(chunk)));
  request.on('end', () => {
    const body = JSON.parse(Buffer.concat(chunks).toString());
    seen.push({ path: request.url!, body });
    response.setHeader('content-type', 'application/json');
    if (request.url === '/v1/query') {
      if (body.target === 'pages') {
        assert.deepEqual(body.query, { record_collection: { page_id: 'board' } });
        response.end(JSON.stringify({ record_collection: { page_id: 'board', writer: { account: 7 }, revision: 0, record_count: 0 } }));
        return;
      }
      assert.equal(body.target, 'runs');
      if (body.query === 'agent_sessions') {
        response.end(JSON.stringify({ agent_sessions: [{ agent_id: 'chief', run_id: 'test-run', actions: 0 }] }));
        return;
      }
      assert.deepEqual(body.query, { model: { query: { agent: { agent_id: 'chief' } } } });
      response.end(JSON.stringify({ model: { agent: { account: 7, agent_id: 'chief', owner: { External: Array(32).fill(7) }, display_name: 'Chief', capability: 'pi', status: 'active', role: 'general', created_at: 0, updated_at: 0, skills: [], recipe_hash: [] } } }));
      return;
    }
    assert.equal(request.headers['x-ducktape-run-action'], 'a'.repeat(64));
    const action = body.message.agent_action;
    assert.equal(action.run_id, 'test-run');
    assert.equal(Buffer.byteLength(action.request_id), 64);
    assert.equal(action.action.operation, 'submit');
    const module = action.action.target.module;
    assert.ok(['pages', 'files'].includes(module));
    const assigned = module === 'files' ? [...Buffer.from(JSON.stringify({ actor: { account: 7 }, source_revision: 1, outcome: { project_snapshot: { snapshot: 'b'.repeat(64) } } }))] : [];
    // The node answers with the receipt id runs derives, under both names.
    const receiptId = `action/${createHash('sha256').update(action.run_id).digest('hex')}/${createHash('sha256').update(action.request_id).digest('hex')}`;
    response.end(JSON.stringify({ receipt_id: receiptId, receipt: {
      request_id: receiptId, operation: 'submit', run_id: action.run_id, target: module, payload: action.action.input,
      status: { completed: { call: 'test-call', outcome: { applied: { output_digest: Array(32).fill(0), assigned } } } },
    } }));
  });
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const address = server.address();
assert.ok(address && typeof address === 'object');
const base = `http://127.0.0.1:${address.port}`;
const child = spawn(binary, ['mcp'], { env: { PATH: process.env.PATH, DUCKTAPE_NODE: base, DUCKTAPE_RUN_ID: 'test-run', DUCKTAPE_RUN_AGENT: 'chief', DUCKTAPE_RUN_ACTION_URL: `${base}/v1/run-action`, DUCKTAPE_RUN_ACTION_TOKEN: 'a'.repeat(64) }, stdio: ['pipe', 'pipe', 'pipe'] });
const pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void }>();
const lines = createInterface({ input: child.stdout });
const ids: number[] = [];
lines.on('line', line => {
  const value = JSON.parse(line);
  const waiter = pending.get(value.id);
  if (!waiter) return;
  pending.delete(value.id);
  if (value.error) waiter.reject(new Error('mcp_rpc_error'));
  else waiter.resolve(value.result);
});
child.on('exit', () => pending.forEach(waiter => waiter.reject(new Error('mcp_exited'))));
const call = (method: string, params: unknown) => new Promise<any>((resolve, reject) => {
  const id = ids.length + 1;
  ids.push(id);
  pending.set(id, { resolve, reject });
  child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
});
const adapter: NetworkToolAdapter = { callTool: (name, args) => Promise.resolve()
  .then(() => call('tools/call', { name, arguments: args }))
  .then(value => { assert.equal(value.isError, false, JSON.stringify(value)); return value; }) };
await Promise.resolve()
  .then(() => call('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'chief-wire-test', version: '1.0.0' } }))
  .then(() => call('tools/list', {}))
  .then(value => {
    assert.ok(value.tools.some((tool: { name: string }) => tool.name === 'ducktape_action'));
    assert.ok(value.tools.some((tool: { name: string }) => tool.name === 'ducktape_query'));
    assert.ok(value.tools.some((tool: { name: string }) => tool.name === 'ducktape_whoami'));
    return createNetworkRpc(adapter).requireBudget('chief', 11)
      .then(() => createNetworkRpc(adapter).query('pages', { record_collection: { page_id: 'board' } }));
  })
  .then(value => {
    assert.equal((value as any).record_collection.revision, 0);
    return createNetworkRpc(adapter).submit('pages', { commit_records: { page_id: 'board', expected_revision: 0, request_id: 'module-request', changes: [], state_changes: [{ put: { key: 'smoke', value: {} } }], metadata: null, artifacts: [] } }, 'long-product-operation'.repeat(8));
  })
  .then(() => createNetworkRpc(adapter).submit('files', { project_snapshot: { snapshot: 'a'.repeat(64), path: '/reports/result.txt' } }, 'project'))
  .then((value: any) => {
    const output = JSON.parse(Buffer.from(value.receipt.status.completed.outcome.applied.assigned).toString('utf8'));
    assert.equal(output.outcome.project_snapshot.snapshot, 'b'.repeat(64));
    assert.equal(seen.length, 5);
  })
  .finally(() => { child.stdin.end(); lines.close(); server.close(); });
await once(child, 'exit');
