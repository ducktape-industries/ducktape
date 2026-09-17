// The package, not the provider, translates Chief policy to the existing generic
// MCP catalog and module wire. The adapter carries authenticated transport only;
// neither credentials nor provider-local files enter canonical policy state.
import { createHash, randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';

import type { Board, ChiefCommand, ChiefResult, ChiefService, EffectIdentity, EffectReceipt, FileRef, OutboxEntry, PreparedEffect } from './contracts.ts';
import { isLive, PolicyError, requirePolicy, runById, validId, validateBoard } from './domain.ts';
import { decisionFromEvent, prepareChiefEvents } from './inputs.ts';
import type { CommittedConversationEvent, PreparedChiefInputs } from './inputs.ts';
import { createNetworkPages, managedRecordTargetMatches } from './network-pages.ts';
import { createChiefService, MAX_CHECKPOINT_ARTIFACT_CLAIMS } from './service.ts';
import { fingerprint } from './store.ts';

// -- Generic MCP transport and public per-install configuration --------------
export interface NetworkToolAdapter {
  callTool(name: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<{
    content: { type: string; text?: string }[]; isError?: boolean;
  }>;
}
export interface NetworkConfig {
  agentId: string; conversationId: string; boardPageId: string;
  inboxPageId: string; homePageId: string; workerAgentId: string;
}
const validateConfig = (config: NetworkConfig): NetworkConfig => {
  const fields = ['agentId', 'conversationId', 'boardPageId', 'inboxPageId', 'homePageId', 'workerAgentId'] as const;
  requirePolicy(config && Object.keys(config).length === fields.length && fields.every(key => validId(config[key])), 'invalid_chief_configuration');
  requirePolicy(config.agentId !== config.workerAgentId, 'chief_cannot_be_its_own_worker');
  return config;
};
const maybeObject = (value: unknown): Record<string, unknown> | undefined => value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
const object = (value: unknown): Record<string, unknown> => {
  requirePolicy(value && typeof value === 'object' && !Array.isArray(value), 'invalid_network_reply');
  return value as Record<string, unknown>;
};
const text = (value: unknown): string => { requirePolicy(typeof value === 'string', 'invalid_network_reply'); return value as string; };
const hash = (value: string): string => createHash('sha256').update(value).digest('hex');
const utf8File = (b64: unknown): string => new TextDecoder('utf-8', { fatal: true }).decode(Buffer.from(text(b64), 'base64'));
// Runs catalog MAX_REQUEST_ID_BYTES is 64, including any prefix.
export const networkRequestId = (operationId: string): string => hash(operationId);
const unwrap = (value: unknown, key: string): unknown => {
  const result = object(value);
  requirePolicy(Object.hasOwn(result, key), 'invalid_network_reply');
  return result[key];
};
const parseResult = (result: Awaited<ReturnType<NetworkToolAdapter['callTool']>>): unknown => {
  requirePolicy(!result.isError, 'network_tool_refused');
  const parts = result.content.filter(part => part.type === 'text');
  requirePolicy(parts.length === 1 && typeof parts[0].text === 'string', 'invalid_network_reply');
  return JSON.parse(parts[0].text!);
};
export const MAX_NATIVE_ACTIONS = 32;
const checkpointProjectionBudget = (payload: unknown): number => {
  try {
    const artifacts = object(JSON.parse(text(payload))).artifacts;
    return Array.isArray(artifacts) && artifacts.length <= MAX_CHECKPOINT_ARTIFACT_CLAIMS ? artifacts.length : 0;
  } catch { return 0; }
};
export const commandWriteBudget = (command: ChiefCommand): number => {
  switch (command.kind) {
    case 'board': case 'report': return 0;
    case 'dispatch': case 'control': return 11;
    case 'recover': return 9;
    case 'change': case 'decision': case 'reconcile': return 4;
  }
};
class RejectedNetworkAction extends PolicyError {
  readonly requestId: string;
  readonly receiptId: string;
  constructor(requestId: string, receiptId: string, reason: Extract<EffectReceipt, { kind: 'rejected' }>['reason']) {
    super(reason);
    this.requestId = requestId;
    this.receiptId = receiptId;
  }
}
// Runs keys an action by the id it derives from the run and the caller's
// request id, and its view repeats that derived id as `request_id`: the
// caller's own id never appears in a receipt. Both receipt names must resolve
// to the one id this write minted.
const actionReceiptId = (runId: string, requestId: string): string => `action/${hash(runId)}/${hash(requestId)}`;
const verifyActionReceipt = (identity: EffectIdentity, receipt: Record<string, unknown>): void => {
  const receiptId = actionReceiptId(identity.runId, identity.requestId);
  const matches = identity.receiptId === receiptId
    && receipt.request_id === receiptId && receipt.run_id === identity.runId && receipt.operation === 'submit'
    && receipt.target === identity.target && fingerprint(receipt.payload) === identity.payloadFingerprint;
  requirePolicy(matches, 'wrong_network_receipt');
};
const completedAction = (receipt: Record<string, unknown>): Extract<EffectReceipt, { kind: 'rejected' }>['reason'] | null => {
  const outcome = object(object(unwrap(receipt.status, 'completed')).outcome);
  requirePolicy(Object.keys(outcome).length === 1, 'unconfirmed_network_operation');
  switch (Object.keys(outcome)[0]) {
    case 'applied': return null;
    case 'rejected':
      requirePolicy(typeof object(outcome.rejected).reason === 'string', 'unconfirmed_network_operation');
      return 'network_action_rejected';
    case 'refused':
      requirePolicy(['not_a_program', 'revoked', 'suspended', 'stale_generation', 'wrong_executor'].includes(outcome.refused as string), 'unconfirmed_network_operation');
      return 'network_action_refused';
    case 'unrepresentable':
      // An unrepresentable APPLIED outcome may have committed effects.
      requirePolicy(['rejected', 'refused'].includes(object(outcome.unrepresentable).attempted as string), 'unconfirmed_network_operation');
      return 'network_rejection_unrepresentable';
    default: throw new PolicyError('unconfirmed_network_operation');
  }
};
const frozenMessage = (input: unknown): unknown => {
  const value: unknown = structuredClone(input);
  const freeze = (item: unknown): unknown => {
    if (item === null || typeof item !== 'object') return item;
    Object.values(item).forEach(freeze);
    return Object.freeze(item);
  };
  return freeze(value);
};
export const createNetworkRpc = (adapter: NetworkToolAdapter) => {
  const scope: { runId?: string } = {};
  const query = (module: string, input: unknown, signal?: AbortSignal): Promise<unknown> => Promise.resolve()
    .then(() => adapter.callTool('ducktape_query', { operation: 'query', target: { module }, input }, signal))
    .then(parseResult);
  const submit = (module: string, input: unknown, id: string, signal?: AbortSignal): Promise<unknown> => Promise.resolve()
    .then(() => adapter.callTool('ducktape_action', { operation: 'submit', target: { module }, input, request_id: networkRequestId(id) }, signal))
    .then(parseResult)
    .then(value => {
      const receipt = object(unwrap(value, 'receipt'));
      const requestId = networkRequestId(id);
      const answersThisRun = typeof receipt.run_id === 'string' && (scope.runId === undefined || receipt.run_id === scope.runId);
      requirePolicy(answersThisRun, 'wrong_network_receipt');
      const receiptId = actionReceiptId(receipt.run_id as string, requestId);
      const answersThisRequest = receipt.request_id === receiptId && object(value).receipt_id === receiptId && receipt.operation === 'submit';
      const answersThisWrite = receipt.target === module && fingerprint(receipt.payload) === fingerprint(input);
      requirePolicy(answersThisRequest && answersThisWrite, 'wrong_network_receipt');
      const reason = completedAction(receipt);
      if (reason === null) return value;
      throw new RejectedNetworkAction(requestId, receiptId, reason);
    });
  const requireBudget = (agentId: string, required: number, signal?: AbortSignal): Promise<void> => Promise.resolve()
    .then(() => adapter.callTool('ducktape_whoami', {}, signal)).then(parseResult)
    .then(value => {
      const identity = object(value);
      requirePolicy(identity.agent_id === agentId && typeof identity.run_id === 'string', 'wrong_action_session');
      scope.runId = identity.run_id as string;
      return query('runs', 'agent_sessions', signal).then(value => {
        const sessions = unwrap(value, 'agent_sessions');
        requirePolicy(Array.isArray(sessions), 'invalid_action_budget');
        const current = (sessions as unknown[]).map(object).filter(session => session.run_id === identity.run_id);
        requirePolicy(current.length === 1 && current[0].agent_id === agentId, 'wrong_action_session');
        const used = current[0].actions;
        requirePolicy(Number.isSafeInteger(used) && (used as number) >= 0 && (used as number) <= MAX_NATIVE_ACTIONS, 'invalid_action_budget');
        requirePolicy((used as number) + required <= MAX_NATIVE_ACTIONS, 'native_action_budget_reserved');
      });
    });
  const currentRunId = (): string => {
    requirePolicy(typeof scope.runId === 'string', 'wrong_action_session');
    return scope.runId!;
  };
  return { query, submit, requireBudget, currentRunId };
};

// -- Scoped immutable Files artifacts; Pages receipts own protected retention --
// Files authorizes writes only under `/home/<label>/**` and `/shared/**`, so
// every Chief evidence artifact lives in the shared namespace.
export const reportPath = (fileId: string): string => `/shared/agents/chief/reports/${fileId}.txt`;
const createFiles = (rpc: ReturnType<typeof createNetworkRpc>) => ({
  read: (ref: FileRef, signal?: AbortSignal): Promise<string> => Promise.resolve()
    .then(() => {
      requirePolicy(/^report-[a-f0-9]{64}$/.test(ref.fileId), 'invalid_artifact_id');
      return rpc.query('files', { read: { path: reportPath(ref.fileId), snapshot: ref.hash, offset: 0, len: 256 * 1024 + 1 } }, signal);
    })
    .then(value => {
      const read = object(unwrap(value, 'read'));
      const content = utf8File(read.b64);
      requirePolicy(read.eof === true && hash(content) === ref.fileId.slice(7), 'artifact_content_mismatch');
      return content;
    }),
  project: (ref: FileRef, signal?: AbortSignal): Promise<FileRef> => Promise.resolve()
    .then(() => {
      requirePolicy(/^report-[a-f0-9]{64}$/.test(ref.fileId) && /^[a-f0-9]{64}$/.test(ref.hash), 'unsupported_artifact_claim');
      return rpc.query('files', { read: { path: reportPath(ref.fileId), snapshot: ref.hash, offset: 0, len: 256 * 1024 + 1 } }, signal);
    })
    .then(value => {
      const read = object(unwrap(value, 'read'));
      const content = utf8File(read.b64);
      requirePolicy(read.eof === true && hash(content) === ref.fileId.slice(7), 'artifact_content_mismatch');
      return rpc.submit('files', { project_snapshot: { snapshot: ref.hash, path: reportPath(ref.fileId) } }, `claim-${randomUUID()}`, signal);
    })
    .then(value => {
      const applied = object(unwrap(object(unwrap(object(unwrap(value, 'receipt')).status, 'completed')).outcome, 'applied'));
      requirePolicy(Array.isArray(applied.assigned), 'files_assignment_missing');
      const output = object(JSON.parse(Buffer.from(applied.assigned as number[]).toString('utf8')));
      const snapshot = text(object(unwrap(output.outcome, 'project_snapshot')).snapshot);
      requirePolicy(/^[a-f0-9]{64}$/.test(snapshot), 'invalid_projected_snapshot');
      return { fileId: ref.fileId, hash: snapshot };
    }),
  put: (input: { operationId: string; content: string }, signal?: AbortSignal): Promise<FileRef> => {
    const digest = hash(input.content);
    const fileId = `report-${digest}`;
    const path = reportPath(fileId);
    const readHead = (): Promise<string> => Promise.resolve()
      .then(() => rpc.query('files', { refs: {} }, signal))
      .then(value => text(object(unwrap(value, 'refs')).head));
    const verify = (snapshot: string): Promise<FileRef> => Promise.resolve()
      .then(() => rpc.query('files', { read: { path, snapshot, offset: 0, len: Buffer.byteLength(input.content) + 1 } }, signal))
      .then(value => {
        const read = object(unwrap(value, 'read'));
        requirePolicy(read.eof === true && utf8File(read.b64) === input.content, 'report_content_mismatch');
        return { fileId, hash: snapshot };
      });
    const project = (snapshot: string): Promise<FileRef> => Promise.resolve()
      .then(() => verify(snapshot))
      // A candidate can expire before Pages commits retention. A new candidate
      // is harmless data, not a repeated worker dispatch; never alias a stale
      // projected-snapshot receipt when retrying an uncommitted policy write.
      .then(() => rpc.submit('files', { project_snapshot: { snapshot, path } }, `${input.operationId}:${randomUUID()}`, signal))
      .then(value => {
        const applied = object(unwrap(object(unwrap(object(unwrap(value, 'receipt')).status, 'completed')).outcome, 'applied'));
        const assigned = applied.assigned;
        requirePolicy(Array.isArray(assigned) && assigned.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255), 'files_assignment_missing');
        const output = object(JSON.parse(Buffer.from(assigned as number[]).toString('utf8')));
        const projected = text(object(unwrap(output.outcome, 'project_snapshot')).snapshot);
        requirePolicy(/^[a-f0-9]{64}$/.test(projected), 'invalid_projected_snapshot');
        return verify(projected);
      });
    return Promise.resolve()
      .then(() => {
        requirePolicy(Buffer.byteLength(input.content) <= 256 * 1024, 'file_exceeds_inline_budget');
        return rpc.query('files', { stat: { path, snapshot: null } }, signal);
      })
      .then(value => {
        if (unwrap(value, 'stat') !== null) return readHead().then(project);
        return Promise.resolve()
          .then(() => rpc.submit('files', { commit: { base_snapshot: null, message: 'Chief immutable worker report', changes: [{ put: { path, exec: false, meta: {}, content: { inline: { b64: Buffer.from(input.content).toString('base64') } } } }] } }, `${input.operationId}:file`, signal))
          .then(readHead).then(project);
      });
  },
});

// -- Independent Jobs + explicit acknowledged controls + generic Runs inbox ---
interface JobView {
  job_id: string; conversation_id: string; status: string; attempt: number;
  spec: string; kind: string; execution: string; submitter: unknown; previous_job_id: string | null;
}
const jobQuery = (rpc: ReturnType<typeof createNetworkRpc>, input: unknown, signal?: AbortSignal): Promise<unknown> => Promise.resolve()
  .then(() => rpc.query('tasks', { job: input }, signal)).then(value => unwrap(value, 'job'));
const getJob = (rpc: ReturnType<typeof createNetworkRpc>, jobId: string, signal?: AbortSignal): Promise<JobView | null> => Promise.resolve()
  .then(() => jobQuery(rpc, { get: { job_id: jobId } }, signal))
  .then(value => unwrap(value, 'job') as JobView | null);
const jobIdFor = (operationId: string): string => `chief-job-${hash(operationId)}`;
const inspectDispatch = (rpc: ReturnType<typeof createNetworkRpc>, entry: OutboxEntry, config: NetworkConfig, account: number, signal?: AbortSignal): Promise<EffectReceipt | null> => Promise.resolve()
  .then(() => getJob(rpc, jobIdFor(entry.operationId), signal))
  .then(job => {
    if (!job) return null;
    requirePolicy(entry.payload.kind === 'dispatch', 'invalid_outbox_payload');
    if (entry.payload.kind !== 'dispatch') throw new PolicyError('invalid_outbox_payload');
    requirePolicy(job.spec === entry.payload.prompt && job.kind === `agent/${config.workerAgentId}` && job.execution === 'conversation' && object(job.submitter).account === account, 'wrong_job_receipt');
    return { kind: 'dispatch', operationId: entry.operationId, jobId: job.job_id, conversationId: job.conversation_id };
  });
const inspectControl = (rpc: ReturnType<typeof createNetworkRpc>, entry: OutboxEntry, signal?: AbortSignal): Promise<EffectReceipt | null> => {
  if (entry.payload.kind !== 'control') throw new PolicyError('invalid_outbox_payload');
  const payload = entry.payload;
  return Promise.resolve()
    .then(() => Promise.all([getJob(rpc, payload.jobId, signal), jobQuery(rpc, { controls: { job_id: payload.jobId } }, signal)]))
    .then(([job, value]) => {
      const controls = unwrap(value, 'controls');
      requirePolicy(Array.isArray(controls), 'invalid_network_reply');
      const control = (controls as Record<string, unknown>[]).find(item => item.operation_id === networkRequestId(entry.operationId));
      if (!job || !control) return null;
      const intended = payload.control === 'steer' ? { steer: { text: payload.text } } : 'cancel';
      requirePolicy(fingerprint(control.input) === fingerprint(intended), 'wrong_control_receipt');
      const acknowledgements = control.acknowledgements as { attempt: number }[];
      requirePolicy(Array.isArray(acknowledgements), 'invalid_network_reply');
      if (!acknowledgements.some(ack => ack.attempt === job.attempt)) return null;
      return { kind: 'control', operationId: entry.operationId, jobId: payload.jobId, status: 'applied' };
    });
};
const inspectWake = (rpc: ReturnType<typeof createNetworkRpc>, entry: OutboxEntry, signal?: AbortSignal): Promise<EffectReceipt | null> => {
  if (entry.payload.kind !== 'wake') throw new PolicyError('invalid_outbox_payload');
  const payload = entry.payload;
  // Events are append-only. Pagination has an explicit continuation; no model
  // wake or board snapshot is generated by this receipt-only traversal.
  const page = (from: number): Promise<EffectReceipt | null> => Promise.resolve()
    .then(() => rpc.query('runs', { conversation_events: { conversation_id: payload.conversationId, from, limit: 32 } }, signal))
    .then(value => {
      const events = unwrap(value, 'conversation_events') as { sequence: number; operation_id: string; input: unknown }[];
      requirePolicy(Array.isArray(events), 'invalid_network_reply');
      const event = events.find(item => item.operation_id === networkRequestId(entry.operationId));
      if (event) {
        const expected = { event: { kind: payload.event, content: { entityId: payload.entityId, operationId: entry.operationId } } };
        requirePolicy(fingerprint(event.input) === fingerprint(expected), 'wrong_wake_receipt');
        return { kind: 'wake', operationId: entry.operationId, inboxId: `${payload.conversationId}:${event.sequence}` };
      }
      if (events.length < 32) return null;
      const next = events.at(-1)!.sequence + 1;
      requirePolicy(next > from, 'invalid_network_cursor');
      return page(next);
    });
  return page(0);
};

// -- Package-owned initialization --------------------------------------------
const installedConfig = (): Promise<NetworkConfig> => Promise.resolve()
  // The only filesystem read is the immutable per-install public configuration.
  .then(() => readFile(new URL('./chief.config.json', import.meta.url), 'utf8'))
  .then(value => validateConfig(JSON.parse(value)));
export interface NetworkChiefService extends ChiefService {
  conversationId: string;
  prepareInputs(events: CommittedConversationEvent[], signal?: AbortSignal): Promise<PreparedChiefInputs>;
}
export const createNetworkService = (adapter: NetworkToolAdapter, configuration?: NetworkConfig): Promise<NetworkChiefService> => Promise.resolve()
  .then(() => configuration ? validateConfig(configuration) : installedConfig())
  .then(config => {
    const rpc = createNetworkRpc(adapter);
    const authority: { account?: number } = {};
    const pages = createNetworkPages(rpc, config.boardPageId, config.conversationId);
    const lookup = (operationId: string, signal?: AbortSignal): Promise<EffectReceipt | null> => Promise.resolve()
      .then(() => pages.read(signal))
      .then(snapshot => {
        const board = snapshot.value as { outbox: OutboxEntry[] };
        const entry = board.outbox.find(item => item.operationId === operationId);
        if (!entry) throw new PolicyError('unknown_operation');
        const identity = entry.effect;
        if (!identity) return null;
        const matchesOperation = identity.requestId === networkRequestId(operationId)
          && identity.target === (entry.payload.kind === 'wake' ? 'runs' : 'tasks')
          && identity.receiptId === actionReceiptId(identity.runId, identity.requestId);
        requirePolicy(matchesOperation, 'wrong_network_receipt');
        // The original action lives under its originating native run, not the
        // current turn. A missing/uncertain action never authorizes another send.
        return rpc.query('runs', { action_request: { request_id: identity.receiptId } }, signal)
          .then(value => {
            const stored = unwrap(value, 'action_request');
            if (stored === null) return null;
            const receipt = object(stored);
            verifyActionReceipt(identity, receipt);
            const reason = completedAction(receipt);
            if (reason !== null) return { kind: 'rejected' as const, operationId, requestId: identity.requestId, receiptId: identity.receiptId, reason };
            switch (entry.payload.kind) {
              case 'dispatch': return inspectDispatch(rpc, entry, config, authority.account!, signal);
              case 'control': return inspectControl(rpc, entry, signal);
              case 'wake': return inspectWake(rpc, entry, signal);
              default: throw new PolicyError('invalid_outbox_payload');
            }
          });
      });
    const prepareJob = (entry: OutboxEntry, signal?: AbortSignal): Promise<unknown> => {
      if (entry.payload.kind !== 'dispatch') throw new PolicyError('invalid_outbox_payload');
      const payload = entry.payload;
      const jobId = jobIdFor(entry.operationId);
      const conversation = payload.conversation;
      if (conversation.kind === 'fresh') return Promise.resolve({ job: { submit_conversation: { job_id: jobId, kind: `agent/${config.workerAgentId}`, spec: payload.prompt } } });
      return Promise.resolve()
        .then(() => jobQuery(rpc, { get_worker: { conversation_id: conversation.conversationId } }, signal))
        .then(value => {
          const worker = object(unwrap(value, 'worker'));
          const executions = worker.executions as JobView[];
          requirePolicy(Array.isArray(executions) && executions.length > 0, 'retained_conversation_unavailable');
          return { job: { continue: { previous_job_id: executions.at(-1)!.job_id, job_id: jobId, operation_id: networkRequestId(entry.operationId), kind: `agent/${config.workerAgentId}`, spec: payload.prompt } } };
        });
    };
    const prepareMessage = (entry: OutboxEntry, signal?: AbortSignal): Promise<unknown> => {
      switch (entry.payload.kind) {
        case 'dispatch': return prepareJob(entry, signal);
        case 'control': return Promise.resolve({ job: { control: { job_id: entry.payload.jobId, operation_id: networkRequestId(entry.operationId), input: entry.payload.control === 'steer' ? { steer: { text: entry.payload.text } } : 'cancel' } } });
        case 'wake': return Promise.resolve({ append_conversation_input: { conversation_id: entry.payload.conversationId, operation_id: networkRequestId(entry.operationId), input: { event: { kind: entry.payload.event, content: { entityId: entry.payload.entityId, operationId: entry.operationId } } } } });
        default: throw new PolicyError('invalid_outbox_payload');
      }
    };
    const service = createChiefService({ pages, files: createFiles(rpc), effects: {
      lookup,
      prepare: (operationId, payload, signal): Promise<PreparedEffect> => Promise.resolve()
        .then(() => prepareMessage({ operationId, payload, status: 'reserved' }, signal))
        .then(message => {
          // Freeze Continue's resolved previous job and all nested input before
          // the attempt CAS. Submission must never consult mutable history again.
          const input = frozenMessage(message);
          const target = payload.kind === 'wake' ? 'runs' : 'tasks';
          const runId = rpc.currentRunId();
          const requestId = networkRequestId(operationId);
          const identity: EffectIdentity = Object.freeze({ receiptId: actionReceiptId(runId, requestId), requestId, runId, target, payloadFingerprint: fingerprint(input) });
          return { identity, submit: (submitSignal?: AbortSignal) => Promise.resolve()
            .then(() => {
              requirePolicy(rpc.currentRunId() === identity.runId, 'wrong_action_session');
              return rpc.submit(target, input, operationId, submitSignal);
            })
            .then(value => {
              requirePolicy(object(value).receipt_id === identity.receiptId, 'wrong_network_receipt');
              verifyActionReceipt(identity, object(unwrap(value, 'receipt')));
              return lookup(operationId, submitSignal);
            })
            .then(receipt => { if (!receipt) throw new PolicyError('effect_receipt_pending'); return receipt; })
            .catch(error => {
              if (!(error instanceof RejectedNetworkAction)) throw error;
              requirePolicy(error.requestId === identity.requestId && error.receiptId === identity.receiptId, 'wrong_network_receipt');
              return { kind: 'rejected' as const, operationId, requestId: identity.requestId, receiptId: identity.receiptId,
                reason: error.code as Extract<EffectReceipt, { kind: 'rejected' }>['reason'] };
            }) };
        }),
    } }, config.conversationId);
    const readBoard = (signal?: AbortSignal) => Promise.resolve().then(() => pages.read(signal)).then(snapshot => validateBoard(snapshot.value, config.conversationId));
    const scheduleCheckin = (signal?: AbortSignal): Promise<void> => Promise.resolve()
      .then(() => Promise.all([readBoard(signal), rpc.query('runs', { conversation_schedules: { conversation_id: config.conversationId } }, signal)]))
      .then(([board, value]) => {
        const schedules = unwrap(value, 'conversation_schedules') as { schedule_id: string; operation_id: string; status: unknown; input: unknown }[];
        requirePolicy(Array.isArray(schedules), 'invalid_schedule_reply');
        const current = schedules.find(schedule => schedule.schedule_id === 'chief-checkin');
        const live = board.runs.some(isLive);
        const minutes = live ? board.checkinMinutes : null;
        const input = { event: { kind: 'chief.checkin', content: { minutes } } };
        const pending = current?.status && typeof current.status === 'object' && Object.hasOwn(current.status, 'pending');
        const alreadyScheduled = minutes !== null && pending && fingerprint(current!.input) === fingerprint(input);
        const alreadyStopped = minutes === null && !pending;
        if (alreadyScheduled || alreadyStopped) return;
        const operationId = networkRequestId(JSON.stringify(['checkin', config.conversationId, board.revision, current?.operation_id, minutes]));
        return rpc.requireBudget(config.agentId, 1, signal)
          .then(() => rpc.submit('runs', { schedule_conversation_input: { conversation_id: config.conversationId, operation_id: operationId, schedule_id: 'chief-checkin', after_secs: minutes === null ? null : minutes * 60, input } }, operationId, signal)).then(() => undefined);
      });
    const inputPlan = (board: Board, events: CommittedConversationEvent[]) => {
      const event = events[0];
      const input = maybeObject(maybeObject(event?.input)?.event);
      const content = maybeObject(input?.content);
      const source = maybeObject(maybeObject(content?.attribution)?.source);
      const snapshot = maybeObject(content?.source);
      const actor = maybeObject(event?.actor)?.Module;
      const jobInput = actor === 'tasks' && input?.kind === 'attribution' && source?.module === 'tasks' && ['job', 'job_event'].includes(source.kind as string);
      if (jobInput) {
        const jobId = snapshot?.job_id;
        const related = board.outbox.filter(entry => {
          if (entry.status !== 'attempted') return false;
          switch (entry.payload.kind) {
            case 'dispatch': return jobIdFor(entry.operationId) === jobId;
            case 'control': return entry.payload.jobId === jobId;
            case 'wake': return false;
          }
        }).slice(0, 1);
        const run = board.runs.find(run => run.jobId === jobId || related.some(entry => entry.payload.kind === 'dispatch' && entry.payload.runId === run.id));
        const live = run && isLive(run);
        const operation = maybeObject(snapshot?.operation);
        const checkpoint = maybeObject(operation?.checkpoint);
        const immutable = source.kind === 'job_event';
        const settlement = operation && ['finalize', 'settle_cancellation', 'cancel'].some(kind => Object.hasOwn(operation, kind));
        const terminal = live && (immutable ? settlement : ['done', 'failed', 'cancelled'].includes(String(snapshot?.status)));
        const report = live && immutable && checkpoint?.kind === 'report';
        const progress = live && immutable && checkpoint?.kind === 'checkpoint' && event.sequence > run.observedSequence;
        const progressWrites = progress ? 5 + checkpointProjectionBudget(checkpoint?.payload) : 0;
        const policyWrites = terminal || report ? 5 : progressWrites;
        return { related, writes: related.length * 3 + policyWrites };
      }
      const freshComment = actor === 'pages' && source?.module === 'pages' && source.kind === 'comment' && snapshot?.mutation === 'created';
      const ask = freshComment && board.asks.find(ask => ask.status === 'open' && managedRecordTargetMatches(config.boardPageId, 'ask', ask.id, String(snapshot?.page_id)));
      return { related: [] as OutboxEntry[], writes: ask ? 3 : 0 };
    };
    const prepareEventInputs = (events: CommittedConversationEvent[], signal?: AbortSignal) => Promise.resolve()
      .then(() => {
        // Runs queues one committed event per turn. Queries may paginate64;
        // that is not permission to spend64 events' writes in one32-action run.
        requirePolicy(events.length <= 1, 'resident_event_batch_exceeds_turn');
        return readBoard(signal);
      })
      .then(board => {
        const plan = inputPlan(board, events);
        const budget = plan.writes === 0 ? Promise.resolve() : rpc.requireBudget(config.agentId, plan.writes + 1, signal);
        // One related receipt(3), raw checkpoint/history(5), at most4
        // file-only claim projections, timer(1): at most13 native actions.
        // Terminal/report events need no claim projections: at most9.
        return budget.then(() => plan.related.reduce<Promise<void>>((previous, entry) => Promise.resolve().then(() => previous)
          .then(() => service.execute({ kind: 'reconcile', operationId: entry.operationId }, signal))
          .then(result => { if (!result.success) throw new PolicyError(result.error); }), Promise.resolve()));
      })
      .then(() => prepareChiefEvents(service, () => readBoard(signal), events, signal, { collectionPageId: config.boardPageId,
        matchesAskTarget: (askId, targetId) => managedRecordTargetMatches(config.boardPageId, 'ask', askId, targetId) }))
      .then(result => scheduleCheckin(signal).then(() => result));
    const findMessage = (messageId: string, from: number, signal?: AbortSignal): Promise<CommittedConversationEvent> => Promise.resolve()
      .then(() => rpc.query('runs', { conversation_events: { conversation_id: config.conversationId, from, limit: 32 } }, signal))
      .then(value => {
        const events = unwrap(value, 'conversation_events') as CommittedConversationEvent[];
        requirePolicy(Array.isArray(events), 'invalid_network_reply');
        const event = events.find(item => {
          const input = object(item.input);
          if (!input.chat) return false;
          return object(object(object(input.chat).message).head).message_id === messageId;
        });
        if (event) return event;
        requirePolicy(events.length === 32 && events.at(-1)!.sequence >= from, 'member_message_unavailable');
        return findMessage(messageId, events.at(-1)!.sequence + 1, signal);
      });
    const recordDecision = (command: Extract<ChiefCommand, { kind: 'decision' }>, signal?: AbortSignal): Promise<ChiefResult> => Promise.resolve()
      .then(() => Promise.all([readBoard(signal), pages.receipt(command.operationId, signal)]))
      .then(([board, prior]) => {
        requirePolicy(prior || board.revision === command.expectedRevision, 'revision_conflict');
        return findMessage(command.messageId, 0, signal);
      })
      .then(event => service.receive(decisionFromEvent(event, config.conversationId, command.askId, command.messageId, command.operationId), signal));
    const recover = (command: Extract<ChiefCommand, { kind: 'recover' }>, signal?: AbortSignal): Promise<ChiefResult> => Promise.resolve()
      .then(() => readBoard(signal))
      .then(board => {
        requirePolicy(board.revision === command.expectedRevision, 'revision_conflict');
        const run = runById(board, command.runId);
        requirePolicy(run.jobId && run.conversationId, 'receipt_unknown_keep_reserved');
        return Promise.resolve()
          .then(() => getJob(rpc, run.jobId!, signal))
          .then(job => {
            if (job) return job;
            return jobQuery(rpc, { get_worker: { conversation_id: run.conversationId } }, signal)
              .then(value => {
                const worker = unwrap(value, 'worker');
                if (!worker) return null;
                const executions = object(worker).executions as JobView[];
                requirePolicy(Array.isArray(executions), 'invalid_network_reply');
                return executions.find(item => item.job_id === run.jobId) ?? null;
              });
          })
          .then(job => {
            if (!job) return service.receive({ kind: 'unavailable', operationId: command.operationId, runId: run.id, jobId: run.jobId! }, signal);
            const event: CommittedConversationEvent = { sequence: 0, operation_id: command.operationId, actor: { Module: 'tasks' }, admitted_at: 0,
              input: { event: { kind: 'attribution', content: { attribution: { source: { module: 'tasks', kind: 'job', object: job.job_id } }, source: job } } } };
            return prepareEventInputs([event], signal).then(() => ({ success: true as const, data: { operationId: command.operationId, runId: run.id, status: job.status, source: 'authoritative_jobs_history' } }));
          });
      });
    const execute = (command: ChiefCommand, signal?: AbortSignal): Promise<ChiefResult> => {
      switch (command.kind) {
        case 'decision': return recordDecision(command, signal);
        case 'recover': return recover(command, signal);
        case 'board': case 'report': case 'change': case 'dispatch': case 'control': case 'reconcile': return service.execute(command, signal);
      }
    };
    // The model can request tools concurrently. Serialize this package's write
    // plans; authoritative session counters survive process/lease retries and
    // public-key fields from their query never enter prompts or stored state.
    let pending: Promise<unknown> = Promise.resolve();
    const exclusive = <T>(operation: () => Promise<T>): Promise<T> => {
      const previous = pending;
      const next = Promise.resolve().then(() => previous).then(operation);
      pending = next.then(() => undefined, () => undefined);
      return next;
    };
    const resident: NetworkChiefService = { ...service, conversationId: config.conversationId,
      prepareInputs: (events, signal) => exclusive(() => prepareEventInputs(events, signal)),
      execute: (command, signal) => exclusive(() => Promise.resolve()
        .then(() => {
          const required = commandWriteBudget(command);
          if (required === 0) return execute(command, signal);
          return rpc.requireBudget(config.agentId, required, signal).then(() => execute(command, signal))
            .then(result => scheduleCheckin(signal).then(() => result));
        })
        .catch(error => ({ success: false as const, error: error instanceof PolicyError ? error.code : 'unconfirmed_network_operation' }))),
    };
    return Promise.resolve()
      .then(() => rpc.query('runs', { conversation: { conversation_id: config.conversationId } }))
      .then(value => {
        const conversation = object(unwrap(value, 'conversation'));
        requirePolicy(conversation.agent_id === config.agentId && Object.hasOwn(object(conversation.source), 'channel'), 'wrong_chief_conversation');
        requirePolicy(Number.isSafeInteger(conversation.account), 'invalid_chief_account');
        authority.account = conversation.account as number;
        return pages.read();
      })
      .then(() => resident);
  });
