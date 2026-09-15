// Pi wiring is tested without native transport, session storage, timers or UI.
// Tool calls are typed commands; lifecycle events cannot sneak in board reads.
import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import type { ExtensionAPI, ExtensionContext, ToolDefinition } from '@earendil-works/pi-coding-agent';
import { Value } from 'typebox/value';

import type { ChiefBridge, ChiefCommand, ChiefControlNotice, ChiefResult } from './contracts.ts';
import { PolicyError } from './domain.ts';
import chiefExtension, { registerChief } from './index.ts';
import { CHIEF_PROMPT } from './prompt.ts';
import { CHIEF_TOOL_NAMES, chiefToolSchemas } from './tools.ts';

// -- Injectable Pi and bridge seams -----------------------------------------
type Handler = (event: Record<string, unknown>, context: Record<string, unknown>) => unknown;
interface CapturedMessage { message: { content: string; customType: string }; options: { triggerTurn?: boolean; deliverAs?: string } }
const fakePi = () => {
  const state = {
    tools: new Map<string, ToolDefinition>(), handlers: new Map<string, Handler[]>(),
    active: [] as string[], messages: [] as CapturedMessage[],
    bus: [] as { name: string; payload: unknown }[],
    busHandlers: new Map<string, Set<(payload: unknown) => void>>(),
  };
  const api = {
    registerTool: (tool: ToolDefinition): void => { state.tools.set(tool.name, tool); },
    on: (event: string, handler: Handler): void => { state.handlers.set(event, [...(state.handlers.get(event) ?? []), handler]); },
    setActiveTools: (names: string[]): void => { state.active = names; },
    sendMessage: (message: CapturedMessage['message'], options: CapturedMessage['options']): void => { state.messages.push({ message, options }); },
    events: {
      emit: (name: string, payload: unknown): void => {
        state.bus.push({ name, payload });
        state.busHandlers.get(name)?.forEach(handler => handler(payload));
      },
      on: (name: string, handler: (payload: unknown) => void): (() => void) => {
        const listeners = state.busHandlers.get(name) ?? new Set<(payload: unknown) => void>();
        state.busHandlers.set(name, listeners);
        listeners.add(handler);
        return () => { listeners.delete(handler); };
      },
    },
  } as unknown as ExtensionAPI;
  const emit = (name: string, event: Record<string, unknown> = {}, context: Record<string, unknown> = {}): Promise<unknown[]> =>
    (state.handlers.get(name) ?? []).reduce<Promise<unknown[]>>((previous, handler) => previous
      .then(results => Promise.resolve().then(() => handler(event, context)).then(result => [...results, result])), Promise.resolve([]));
  const call = (name: string, input: Record<string, unknown>, signal?: AbortSignal) => {
    const tool = state.tools.get(name);
    assert(tool, `Missing tool ${name}`);
    return tool.execute('call', input, signal, undefined, {} as ExtensionContext);
  };
  return { api, state, emit, call };
};
const fakeBridge = () => {
  const state = {
    calls: [] as { command: ChiefCommand; signal?: AbortSignal }[],
    listeners: [] as ((notice: ChiefControlNotice) => void)[], stops: 0,
    result: { success: true, data: { revision: 1 } } as ChiefResult,
    error: undefined as unknown,
  };
  const bridge: ChiefBridge = {
    execute: async (command, signal) => {
      state.calls.push({ command, signal });
      if (state.error) throw state.error;
      return state.result;
    },
    subscribeControl: listener => {
      state.listeners.push(listener);
      return () => { state.stops++; };
    },
  };
  return { bridge, state };
};

// -- Coordination-only lifecycle --------------------------------------------
test('registration/start/prompt hooks never read the board or inject snapshots', async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  assert.equal(pi.state.tools.size, 18);
  assert.deepEqual([...pi.state.tools.keys()].sort(), [...CHIEF_TOOL_NAMES].sort());
  assert.equal(bridge.state.listeners.length, 0);
  await pi.emit('session_start');
  assert.deepEqual(pi.state.active, CHIEF_TOOL_NAMES);
  assert.equal(bridge.state.listeners.length, 1);
  const results = await pi.emit('before_agent_start', { systemPrompt: 'native system prompt' });
  assert.deepEqual(results, [{ systemPrompt: `native system prompt\n\n${CHIEF_PROMPT}` }]);
  assert.equal(bridge.state.calls.length, 0);
  assert.equal(pi.state.messages.length, 0);
  for (const toolName of ['bash', 'agent.call', 'write', 'chief_unknown', 'worker_checkpoint']) {
    const [result] = await pi.emit('tool_call', { toolName });
    assert.equal((result as { block: boolean }).block, true);
  }
  assert.deepEqual(await pi.emit('tool_call', { toolName: 'chief_board' }), [undefined]);
});

test('only compact control identities append without waking; stale callbacks stop at shutdown', async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  await pi.emit('session_start');
  const listener = bridge.state.listeners[0];
  listener({ operationId: 'op1', revision: 3, kind: 'dispatch', entityId: 'run1', rawReport: 'private report secret' } as ChiefControlNotice);
  assert.equal(pi.state.messages.length, 1);
  assert.deepEqual(pi.state.messages[0].options, { triggerTurn: false, deliverAs: 'nextTurn' });
  assert.equal(pi.state.messages[0].message.customType, 'chief-control');
  assert(!pi.state.messages[0].message.content.includes('private report secret'));
  assert.equal(bridge.state.calls.length, 0);
  await pi.emit('session_shutdown');
  assert.equal(bridge.state.stops, 1);
  listener({ operationId: 'late', revision: 4, kind: 'dispatch', entityId: 'run1' });
  assert.equal(pi.state.messages.length, 1);
  await pi.emit('session_shutdown');
  assert.equal(bridge.state.stops, 1);
});

test('default requests only the generic adapter; missing readiness is cancellable and never executes tools', async () => {
  const pi = fakePi();
  chiefExtension(pi.api);
  assert.equal(pi.state.bus.length, 0);
  await pi.emit('session_start');
  assert.equal(pi.state.bus.length, 1);
  assert.equal(pi.state.bus[0].name, 'ducktape:network:bind');
  assert.equal(typeof (pi.state.bus[0].payload as { accept: unknown }).accept, 'function');
  assert.deepEqual(pi.state.active, CHIEF_TOOL_NAMES);
  const controller = new AbortController();
  const pending = pi.emit('before_agent_start', { systemPrompt: 'native' }, { signal: controller.signal });
  controller.abort(new Error('must-not-leak-a-secret'));
  await assert.rejects(pending, /^Error: chief_wait_cancelled$/);
  await pi.emit('session_shutdown');
  assert.equal(pi.state.messages.length, 0);
});

test('resident preparation synchronously accepts exactly one promise and never falls back to a raw prompt', async () => {
  const pi = fakePi();
  chiefExtension(pi.api);
  await pi.emit('session_start');
  const handlers = pi.state.busHandlers.get('ducktape:resident:prepare');
  assert(handlers);
  assert.equal(handlers.size, 1);
  const handler = [...handlers][0];
  const accepted: Promise<unknown>[] = [];
  const returned = handler({ conversation_id: 'conversation', turn_id: 'turn-1', events: [], accept: (promise: Promise<unknown>) => { accepted.push(promise); } });
  assert.equal(returned, undefined);
  assert.equal(accepted.length, 1);
  assert(accepted[0] instanceof Promise);
  const rejected = assert.rejects(accepted[0], /^Error: chief_input_preparation_failed$/);
  await pi.emit('session_shutdown');
  await rejected;
  assert.equal(handlers.size, 0);
  assert.equal(pi.state.messages.length, 0);
  assert.equal(pi.state.bus.length, 1);
});

// -- Exact commands and argument validation ---------------------------------
test('every tool maps to a typed command and forwards the same signal', async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  const mutation = { operationId: 'operation', expectedRevision: 1 };
  const ref = { fileId: 'report1', hash: 'a'.repeat(64) };
  const task = { id: 'task1', key: 'canonical key', title: 'title', brief: 'brief', scope: ['src'], access: 'read', dependencies: [], origin: 'task0' };
  const ask = { id: 'ask1', key: 'ask key', title: 'title', question: 'question', whyMember: 'authority', ifUnasked: 'cost', recommendation: 'recommended', options: [], artifacts: [], blocks: [], sources: [], addressedTo: ['member1'] };
  const cases: { name: keyof typeof chiefToolSchemas; input: Record<string, unknown>; expected: unknown }[] = [
    { name: 'chief_board', input: { section: 'tasks', offset: 0, limit: 1, id: 'task1', detailOffset: 2 }, expected: { kind: 'board', section: 'tasks', offset: 0, limit: 1, id: 'task1', detailOffset: 2 } },
    { name: 'chief_report', input: { runId: 'run1' }, expected: { kind: 'report', runId: 'run1' } },
    { name: 'chief_task', input: { ...mutation, task }, expected: { kind: 'change', ...mutation, action: { kind: 'task_put', task } } },
    { name: 'chief_transition', input: { ...mutation, taskId: 'task1', status: 'blocked', reason: 'reason', footprint: ['src/one.ts'] }, expected: { kind: 'change', ...mutation, action: { kind: 'task_status', taskId: 'task1', status: 'blocked', reason: 'reason', footprint: ['src/one.ts'] } } },
    { name: 'chief_merge', input: { ...mutation, sourceId: 'task1', targetId: 'task2' }, expected: { kind: 'change', ...mutation, action: { kind: 'task_merge', sourceId: 'task1', targetId: 'task2' } } },
    { name: 'chief_accept', input: { ...mutation, taskId: 'task1', outcome: 'verified', evidence: [ref], footprint: ['src/one.ts'] }, expected: { kind: 'change', ...mutation, action: { kind: 'accept', taskId: 'task1', outcome: 'verified', evidence: [ref], footprint: ['src/one.ts'] } } },
    { name: 'chief_ask_open', input: { ...mutation, ask }, expected: { kind: 'change', ...mutation, action: { kind: 'ask_open', ask } } },
    { name: 'chief_decision', input: { ...mutation, askId: 'ask1', messageId: 'message1' }, expected: { kind: 'decision', ...mutation, askId: 'ask1', messageId: 'message1' } },
    { name: 'chief_ask_resolve', input: { ...mutation, askId: 'ask1', status: 'answered', resolution: 'acted' }, expected: { kind: 'change', ...mutation, action: { kind: 'ask_resolve', askId: 'ask1', status: 'answered', resolution: 'acted' } } },
    { name: 'chief_rule_put', input: { ...mutation, rule: { id: 'rule1', when: 'when', instruction: 'do' } }, expected: { kind: 'change', ...mutation, action: { kind: 'rule_put', rule: { id: 'rule1', when: 'when', instruction: 'do' } } } },
    { name: 'chief_rule_remove', input: { ...mutation, ruleId: 'rule1' }, expected: { kind: 'change', ...mutation, action: { kind: 'rule_remove', ruleId: 'rule1' } } },
    { name: 'chief_limit', input: { ...mutation, limit: null }, expected: { kind: 'change', ...mutation, action: { kind: 'limit', limit: null } } },
    { name: 'chief_dispatch', input: { ...mutation, taskId: 'task1', fresh: true }, expected: { kind: 'dispatch', ...mutation, taskId: 'task1', fresh: true } },
    { name: 'chief_control', input: { ...mutation, runId: 'run1', control: 'steer', text: 'replacement' }, expected: { kind: 'control', ...mutation, runId: 'run1', control: 'steer', text: 'replacement' } },
    { name: 'chief_reconcile', input: { operationId: 'operation' }, expected: { kind: 'reconcile', operationId: 'operation' } },
    { name: 'chief_checkpoint', input: { ...mutation, focus: 'focus', nextActions: ['act'] }, expected: { kind: 'change', ...mutation, action: { kind: 'checkpoint', focus: 'focus', nextActions: ['act'] } } },
    { name: 'chief_checkin', input: { ...mutation, minutes: 10 }, expected: { kind: 'change', ...mutation, action: { kind: 'checkin', minutes: 10 } } },
    { name: 'chief_recover', input: { ...mutation, runId: 'run1' }, expected: { kind: 'recover', ...mutation, runId: 'run1' } },
  ];
  const signal = new AbortController().signal;
  await cases.reduce((previous, item) => previous.then(async () => {
    assert(Value.Check(chiefToolSchemas[item.name], item.input), item.name);
    const result = await pi.call(item.name, item.input, signal);
    assert.deepEqual(bridge.state.calls.at(-1), { command: item.expected, signal }, item.name);
    assert.equal((JSON.parse((result.content[0] as { text: string }).text) as { success: boolean }).success, true);
  }), Promise.resolve());
  assert.equal(bridge.state.calls.length, 18);
  assert(!Value.Check(chiefToolSchemas.chief_board, { section: 'tasks', offset: 0, limit: 26 }));
  assert(!Value.Check(chiefToolSchemas.chief_transition, { ...mutation, taskId: 'task1', status: 'done', reason: 'x' }));
  assert(!Value.Check(chiefToolSchemas.chief_decision, { ...mutation, askId: 'ask1', messageId: 'message1', memberId: 'forged' }));
  assert(!Value.Check(chiefToolSchemas.chief_control, { ...mutation, runId: 'run1', control: 'cancel', text: '' }));
  assert(!Object.hasOwn(chiefToolSchemas.chief_dispatch.properties, 'runId'));
  assert(!Value.Check(chiefToolSchemas.chief_dispatch, { ...mutation, taskId: 'task1', fresh: true, runId: mutation.operationId }), 'Even a matching caller-supplied runId is not part of dispatch');
  const { origin: _origin, ...unattributed } = task;
  assert(!Value.Check(chiefToolSchemas.chief_task, { ...mutation, task: unattributed }), 'A task without recorded provenance is not a task');
  assert(!Object.hasOwn(chiefToolSchemas.chief_merge.properties, 'footprint'), 'A merge records no footprint; the surviving task does');
});

test("the 'user' sentinel records no origin and never erases one", async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  const mutation = { operationId: 'operation', expectedRevision: 1 };
  const task = { id: 'task1', key: 'canonical key', title: 'title', brief: 'brief', scope: ['src'], access: 'read' as const, dependencies: [] };
  await pi.call('chief_task', { ...mutation, task: { ...task, origin: 'user' } });
  assert.deepEqual(bridge.state.calls.at(-1)?.command, { kind: 'change', ...mutation, action: { kind: 'task_put', task } });
  await pi.call('chief_task', { ...mutation, task: { ...task, origin: 'task0' } });
  assert.deepEqual(bridge.state.calls.at(-1)?.command, { kind: 'change', ...mutation, action: { kind: 'task_put', task: { ...task, origin: 'task0' } } });
});

test('the reader forwards the exact artifact and immutable receipt anchor with its cursor', async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  const input = { runId: 'run1', artifact: { fileId: 'report1', hash: 'a'.repeat(64) },
    anchor: { operationId: 'report-operation', history: { fileId: 'history1', hash: 'b'.repeat(64) } }, offset: 2000, limit: 1000 };
  assert(Value.Check(chiefToolSchemas.chief_report, input));
  await pi.call('chief_report', input);
  assert.deepEqual(bridge.state.calls[0].command, { kind: 'report', ...input });
});

test('aborted tools never call the bridge and failures cannot leak transport/config secrets', async () => {
  const pi = fakePi();
  const bridge = fakeBridge();
  registerChief(pi.api, bridge.bridge);
  await assert.rejects(pi.call('chief_report', { runId: 'run1' }, AbortSignal.abort('private abort secret')), /^Error: chief_operation_failed$/);
  assert.equal(bridge.state.calls.length, 0);
  bridge.state.error = new Error('transport credential SECRET');
  await assert.rejects(pi.call('chief_report', { runId: 'run1' }), /^Error: chief_operation_failed$/);
  bridge.state.error = new PolicyError('known_policy_code');
  await assert.rejects(pi.call('chief_report', { runId: 'run1' }), /^Error: known_policy_code$/);
  bridge.state.error = undefined;
  bridge.state.result = { success: false, error: 'Bearer SECRET' };
  await assert.rejects(pi.call('chief_report', { runId: 'run1' }), /^Error: chief_operation_failed$/);
  bridge.state.result = { success: false, error: 'revision_conflict' };
  await assert.rejects(pi.call('chief_report', { runId: 'run1' }), /^Error: revision_conflict$/);
  bridge.state.result = { success: true, data: { records: 'x'.repeat(50000) } };
  const result = await pi.call('chief_report', { runId: 'run1' });
  assert.deepEqual(result.details, { success: true, truncated: true });
  const text = (result.content[0] as { text: string }).text;
  assert(text.includes('Output truncated'));
  assert(Buffer.byteLength(text, 'utf8') <= 24000);
  assert(text.split('\n').length <= 500);
  assert.equal(pi.state.messages.length, 0);
});
