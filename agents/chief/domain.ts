// Pure Chief policy adapted from pi-chief at a5f1f09 (domain.ts and
// worker-progress.ts). Network identity replaces repo/session paths; no legacy
// decoders. Reports are claims until Chief explicitly accepts reviewed evidence.
import type {
  Ask, Board, DomainAction, EffectIdentity, EffectReceipt, FileRef, OutboxEntry, Progress, Run, Task,
} from './contracts.ts';

// -- Boundary guards --------------------------------------------------------
export class PolicyError extends Error {
  readonly code: string;
  constructor(code: string) { super(code); this.code = code; }
}
export const requirePolicy = (condition: unknown, code: string): void => {
  if (!condition) throw new PolicyError(code);
};
export const assertNever = (value: never): never => { throw new PolicyError('invalid_discriminant'); };
export const boundedText = (value: unknown, max: number): value is string =>
  typeof value === 'string' && value.trim().length > 0 && value.length <= max && !/[\x00-\x08\x0b\x0c\x0e-\x1f]/.test(value);
const validEffect = (effect: EffectIdentity | undefined): boolean => Boolean(effect && boundedText(effect.runId, 1024)
  && /^[a-f0-9]{64}$/.test(effect.requestId) && /^[a-f0-9]{64}$/.test(effect.payloadFingerprint)
  && /^action\/[a-f0-9]{64}\/[a-f0-9]{64}$/.test(effect.receiptId));
export const validId = (value: unknown): value is string =>
  typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._:-]{0,199}$/.test(value);
export const canonicalKey = (value: string): string => value.trim().replace(/\s+/g, ' ').toLowerCase();
const unique = <T>(values: T[]): T[] => [...new Set(values)];
const uniqueIds = (values: string[]): boolean => values.every(validId) && unique(values).length === values.length;
export const validRefs = (values: unknown): values is FileRef[] => Array.isArray(values) && values.length <= 16
  && values.every(value => value && validId(value.fileId) && /^[a-f0-9]{64}$/.test(value.hash));
const unionRefs = (a: FileRef[], b: FileRef[]): FileRef[] => [...new Map([...a, ...b].map(ref => [ref.fileId + ':' + ref.hash, ref])).values()];
export const isLive = (run: Run): boolean => ['reserved', 'queued', 'running'].includes(run.status);
export const taskById = (board: Board, id: string): Task => {
  const task = board.tasks.find(item => item.id === id);
  if (!task) throw new PolicyError('unknown_task');
  return task;
};
export const runById = (board: Board, id: string): Run => {
  const run = board.runs.find(item => item.id === id);
  if (!run) throw new PolicyError('unknown_run');
  return run;
};
const askById = (board: Board, id: string): Ask => {
  const ask = board.asks.find(item => item.id === id);
  if (!ask) throw new PolicyError('unknown_ask');
  return ask;
};
export const canonicalTask = (board: Board, id: string, seen: string[] = []): Task => {
  requirePolicy(!seen.includes(id), 'merged_cycle');
  const task = taskById(board, id);
  return task.mergedInto ? canonicalTask(board, task.mergedInto, [...seen, id]) : task;
};
const noLive = (board: Board, id: string): void => requirePolicy(!board.runs.some(run => run.taskId === id && isLive(run)), 'live_worker');
const replaceTask = (board: Board, task: Task): Board => ({ ...board, tasks: board.tasks.map(item => item.id === task.id ? task : item) });
const replaceRun = (board: Board, run: Run): Board => ({ ...board, runs: board.runs.map(item => item.id === run.id ? run : item) });
const pendingAsk = (ask: Ask): boolean => ['open', 'replied'].includes(ask.status);
const dependenciesDone = (board: Board, task: Task): boolean => task.dependencies.every(id => canonicalTask(board, id).status === 'done');

// -- Authoritative-state validation -----------------------------------------
const checkTask = (task: Task): void => {
  requirePolicy(validId(task.id) && boundedText(task.key, 200) && task.key === canonicalKey(task.key), 'invalid_task_key');
  requirePolicy(boundedText(task.title, 300) && boundedText(task.brief, 12000), 'invalid_task_text');
  requirePolicy(Array.isArray(task.scope) && task.scope.length > 0 && task.scope.length <= 32, 'invalid_scope');
  requirePolicy(task.scope.every(path => boundedText(path, 300) && !path.startsWith('/') && !path.includes('\\') && !path.split('/').includes('..')), 'invalid_scope');
  requirePolicy(['read', 'write'].includes(task.access) && Array.isArray(task.dependencies) && task.dependencies.length <= 64 && uniqueIds(task.dependencies), 'invalid_dependencies');
  requirePolicy(['queued', 'running', 'review', 'done', 'blocked', 'cancelled', 'merged'].includes(task.status), 'invalid_task_status');
  requirePolicy(validRefs(task.evidence), 'invalid_evidence');
  requirePolicy(task.status !== 'blocked' || boundedText(task.reason, 2000), 'missing_block_reason');
  requirePolicy((task.status === 'merged') === Boolean(task.mergedInto), 'invalid_merge');
  requirePolicy(task.status !== 'done' || Boolean(task.acceptance), 'missing_acceptance');
  if (!task.acceptance) return;
  const acceptedState = ['done', 'merged'].includes(task.status);
  const acceptedRefs = validRefs(task.acceptance.evidence) && task.acceptance.evidence.length > 0;
  requirePolicy(acceptedState && acceptedRefs && boundedText(task.acceptance.outcome, 2000), 'invalid_acceptance');
  requirePolicy(Number.isSafeInteger(task.acceptance.revision) && task.acceptance.revision > 0 && validId(task.acceptance.runId), 'invalid_acceptance_revision');
  requirePolicy(task.acceptance.evidence.every(ref => task.evidence.some(item => item.fileId === ref.fileId && item.hash === ref.hash)), 'acceptance_not_in_evidence');
};
const checkGraph = (board: Board): void => {
  // Shared visitation bounds work on diamond DAGs; local sets are not state.
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const visit = (task: Task): void => {
    requirePolicy(!visiting.has(task.id), 'dependency_cycle');
    if (visited.has(task.id)) return;
    visiting.add(task.id);
    task.dependencies.forEach(id => visit(canonicalTask(board, id)));
    visiting.delete(task.id);
    visited.add(task.id);
  };
  board.tasks.filter(task => task.status !== 'merged').forEach(visit);
};
const checkAsk = (board: Board, ask: Ask): void => {
  requirePolicy(validId(ask.id) && boundedText(ask.key, 200) && ask.key === canonicalKey(ask.key), 'invalid_ask_key');
  requirePolicy(boundedText(ask.title, 300) && boundedText(ask.question, 2000) && boundedText(ask.whyMember, 1000) && boundedText(ask.ifUnasked, 1000) && boundedText(ask.recommendation, 2000), 'invalid_ask_text');
  requirePolicy(Array.isArray(ask.options) && ask.options.length <= 9 && ask.options.every(option => boundedText(option.label, 200) && boundedText(option.consequence, 500)), 'invalid_ask_options');
  requirePolicy(validRefs(ask.artifacts) && uniqueIds(ask.blocks) && uniqueIds(ask.addressedTo) && ask.addressedTo.length > 0, 'invalid_ask_members');
  ask.blocks.forEach(id => taskById(board, id));
  requirePolicy(Array.isArray(ask.sources) && ask.sources.length <= 16, 'invalid_ask_sources');
  ask.sources.forEach(source => {
    const run = board.runs.find(item => item.id === source.runId);
    requirePolicy(run || !pendingAsk(ask), 'invalid_ask_provenance');
    if (run) requirePolicy(run.taskId === source.taskId && run.conversationId === source.conversationId, 'invalid_ask_provenance');
  });
  requirePolicy(['open', 'replied', 'answered', 'superseded', 'dismissed'].includes(ask.status), 'invalid_ask_status');
  requirePolicy(ask.status !== 'replied' || Boolean(ask.reply), 'missing_member_reply');
  requirePolicy(ask.status !== 'open' || !ask.reply, 'reply_already_saved');
  if (!ask.reply) return;
  const { source, text, decisionId } = ask.reply;
  requirePolicy(['chat', 'page_comment'].includes(source.kind) && validId(source.memberId) && validId(source.conversationId) && validId(source.messageId) && validId(decisionId) && boundedText(text, 4000), 'invalid_member_reply');
  requirePolicy(ask.addressedTo.includes(source.memberId), 'wrong_member');
};
export const validateBoard = (value: unknown, conversationId: string): Board => {
  requirePolicy(value && typeof value === 'object' && !Array.isArray(value), 'invalid_board');
  const board = value as Board;
  requirePolicy(validId(conversationId) && board.conversationId === conversationId, 'wrong_conversation');
  requirePolicy(Number.isSafeInteger(board.revision) && board.revision >= 0, 'invalid_revision');
  requirePolicy(board.concurrencyLimit === null || Number.isSafeInteger(board.concurrencyLimit) && board.concurrencyLimit > 0, 'invalid_limit');
  requirePolicy(board.checkinMinutes === null || Number.isSafeInteger(board.checkinMinutes) && board.checkinMinutes >= 1 && board.checkinMinutes <= 1440, 'invalid_checkin');
  requirePolicy(board.checkpoint && typeof board.checkpoint.focus === 'string' && board.checkpoint.focus.length <= 4000 && Array.isArray(board.checkpoint.nextActions) && board.checkpoint.nextActions.length <= 16 && board.checkpoint.nextActions.every(action => boundedText(action, 500)), 'invalid_checkpoint');
  requirePolicy(['tasks', 'runs', 'asks', 'rules', 'outbox'].every(key => Array.isArray(board[key as keyof Board])), 'invalid_board_arrays');
  requirePolicy(board.tasks.length <= 1000 && board.runs.length <= 5000 && board.asks.length <= 1000 && board.rules.length <= 100 && board.outbox.length <= 10000, 'board_capacity');
  requirePolicy(uniqueIds(board.tasks.map(task => task.id)) && uniqueIds(board.runs.map(run => run.id)) && uniqueIds(board.asks.map(ask => ask.id)) && uniqueIds(board.rules.map(rule => rule.id)), 'duplicate_id');
  requirePolicy(unique(board.tasks.map(task => task.key)).length === board.tasks.length, 'duplicate_task_key');
  const asks = board.asks.filter(pendingAsk);
  requirePolicy(unique(asks.map(ask => ask.key)).length === asks.length, 'duplicate_ask_key');
  requirePolicy(uniqueIds(board.outbox.map(entry => entry.operationId)), 'duplicate_outbox_operation');
  board.outbox.forEach(entry => {
    const untouchedReservation = entry.status === 'reserved' && entry.attemptId === undefined && entry.effect === undefined;
    const completeAttempt = entry.status === 'attempted' && validId(entry.attemptId) && validEffect(entry.effect)
      && entry.effect!.target === (entry.payload.kind === 'wake' ? 'runs' : 'tasks');
    requirePolicy(untouchedReservation || completeAttempt, 'invalid_outbox_attempt');
  });
  board.tasks.forEach(task => { checkTask(task); canonicalTask(board, task.id); });
  checkGraph(board);
  const live = board.runs.filter(isLive);
  requirePolicy(unique(live.map(run => run.taskId)).length === live.length, 'duplicate_live_run');
  board.runs.forEach(run => {
    const task = taskById(board, run.taskId);
    requirePolicy(validId(run.id) && run.id === run.operationId && Number.isSafeInteger(run.observedSequence) && run.observedSequence >= -1, 'invalid_run');
    requirePolicy(['reserved', 'queued', 'running', 'completed', 'failed', 'cancelled', 'interrupted'].includes(run.status), 'invalid_run_status');
    requirePolicy(!isLive(run) || task.status === 'running' && task.currentRun === run.id && dependenciesDone(board, task), 'invalid_live_run');
    requirePolicy(run.status !== 'completed' || run.report && validRefs([run.report]), 'missing_report');
    requirePolicy(run.status !== 'interrupted' || boundedText(run.reason, 2000), 'missing_interruption_reason');
    if (run.progress) validateProgress(run.progress);
  });
  board.tasks.forEach(task => {
    requirePolicy(task.status !== 'running' || live.some(run => run.taskId === task.id), 'missing_live_run');
    if (task.currentRun) requirePolicy(runById(board, task.currentRun).taskId === task.id, 'wrong_current_run');
  });
  board.asks.forEach(ask => checkAsk(board, ask));
  board.rules.forEach(checkRule);
  requirePolicy(uniqueIds(board.outbox.map(op => op.operationId)), 'duplicate_operation');
  requirePolicy(board.history === null || validRefs([board.history]), 'invalid_history_ref');
  board.outbox.forEach(entry => {
    requirePolicy(['reserved', 'attempted'].includes(entry.status), 'invalid_outbox_status');
    requirePolicy(['dispatch', 'control', 'wake'].includes(entry.payload.kind), 'invalid_outbox_payload');
  });
  return board;
};
export const emptyBoard = (conversationId: string): Board => validateBoard({ conversationId, revision: 0, tasks: [], runs: [], asks: [], rules: [], outbox: [], history: null, concurrencyLimit: null, checkpoint: { focus: '', nextActions: [] }, checkinMinutes: 10 }, conversationId);

// -- Task and inbox transitions ----------------------------------------------
const putTask = (board: Board, action: Extract<DomainAction, { kind: 'task_put' }>): Board => {
  const spec = { ...action.task, key: canonicalKey(action.task.key) };
  const existing = board.tasks.find(task => task.id === spec.id);
  const sameKey = board.tasks.find(task => task.key === spec.key);
  requirePolicy(!sameKey || sameKey.id === spec.id, 'canonical_key_exists');
  if (!existing) return { ...board, tasks: [...board.tasks, { ...spec, status: 'queued', evidence: [] }] };
  noLive(board, existing.id);
  requirePolicy(['queued', 'blocked'].includes(existing.status), 'reopen_before_edit');
  return replaceTask(board, { ...existing, ...spec });
};
const setStatus = (board: Board, action: Extract<DomainAction, { kind: 'task_status' }>): Board => {
  const task = taskById(board, action.taskId);
  noLive(board, task.id);
  requirePolicy(task.status !== 'merged', 'merged_terminal');
  const { acceptance: _acceptance, reason: _reason, currentRun: _run, ...rest } = task;
  return replaceTask(board, { ...rest, status: action.status, reason: action.reason });
};
const mergeTasks = (board: Board, action: Extract<DomainAction, { kind: 'task_merge' }>): Board => {
  const source = taskById(board, action.sourceId);
  const target = taskById(board, action.targetId);
  requirePolicy(source.id !== target.id && source.status !== 'merged' && ['queued', 'blocked'].includes(target.status), 'invalid_merge_target');
  noLive(board, source.id); noLive(board, target.id);
  const rewire = (ids: string[], owner: string): string[] => unique(ids.map(id => canonicalTask(board, id).id === source.id ? target.id : id)).filter(id => canonicalTask(board, id).id !== owner);
  return { ...board, tasks: board.tasks.map(task => {
    if (task.id === source.id) return { ...task, status: 'merged', mergedInto: target.id };
    if (task.id === target.id) return { ...task, brief: `${target.brief}\n\n${source.title}\n${source.brief}`, evidence: unionRefs(target.evidence, source.evidence), scope: unique([...target.scope, ...source.scope]), access: source.access === 'write' ? 'write' : target.access, dependencies: rewire([...target.dependencies, ...source.dependencies], target.id) };
    return { ...task, dependencies: rewire(task.dependencies, task.id) };
  }) };
};
const acceptTask = (board: Board, action: Extract<DomainAction, { kind: 'accept' }>): Board => {
  const task = taskById(board, action.taskId);
  noLive(board, task.id);
  requirePolicy(task.status === 'review' && validRefs(action.evidence) && action.evidence.length > 0 && boundedText(action.outcome, 2000), 'review_evidence_required');
  requirePolicy(task.currentRun, 'review_run_required');
  const { currentRun, ...rest } = task;
  return replaceTask(board, { ...rest, status: 'done', evidence: unionRefs(task.evidence, action.evidence), acceptance: { revision: board.revision + 1, runId: currentRun!, outcome: action.outcome, evidence: action.evidence } });
};
const openAsk = (board: Board, action: Extract<DomainAction, { kind: 'ask_open' }>): Board => ({ ...board, asks: [...board.asks, { ...action.ask, key: canonicalKey(action.ask.key), status: 'open' }] });
const resolveAsk = (board: Board, action: Extract<DomainAction, { kind: 'ask_resolve' }>): Board => {
  const ask = askById(board, action.askId);
  requirePolicy(pendingAsk(ask) && boundedText(action.resolution, 4000), 'ask_already_resolved');
  requirePolicy(action.status !== 'answered' || ask.status === 'replied', 'member_reply_required');
  return { ...board, asks: board.asks.map(item => item.id === ask.id ? { ...item, status: action.status, resolution: action.resolution } : item) };
};
const checkRule = (rule: Board['rules'][number]): void => {
  requirePolicy(validId(rule.id) && boundedText(rule.when, 1000) && boundedText(rule.instruction, 2000), 'invalid_rule');
  const incident = /\b(last\s+time|yesterday|incidents?|post[ -]?mortems?|after\s+we)\b|\b\d{4}[-/]\d{1,2}[-/]\d{1,2}\b/i;
  requirePolicy(!incident.test(`${rule.when}\n${rule.instruction}`), 'rule_must_be_reusable');
};
const putRule = (board: Board, action: Extract<DomainAction, { kind: 'rule_put' }>): Board => ({ ...board, rules: [...board.rules.filter(rule => rule.id !== action.rule.id), action.rule] });
const removeRule = (board: Board, action: Extract<DomainAction, { kind: 'rule_remove' }>): Board => ({ ...board, rules: board.rules.filter(rule => rule.id !== action.ruleId) });
const setLimit = (board: Board, action: Extract<DomainAction, { kind: 'limit' }>): Board => ({ ...board, concurrencyLimit: action.limit });
const setCheckpoint = (board: Board, action: Extract<DomainAction, { kind: 'checkpoint' }>): Board => ({ ...board, checkpoint: { focus: action.focus, nextActions: action.nextActions } });
const setCheckin = (board: Board, action: Extract<DomainAction, { kind: 'checkin' }>): Board => ({ ...board, checkinMinutes: action.minutes });
export const decide = (board: Board, action: DomainAction): Board => {
  switch (action.kind) {
    case 'task_put': return putTask(board, action);
    case 'task_status': return setStatus(board, action);
    case 'task_merge': return mergeTasks(board, action);
    case 'accept': return acceptTask(board, action);
    case 'ask_open': return openAsk(board, action);
    case 'ask_resolve': return resolveAsk(board, action);
    case 'rule_put': return putRule(board, action);
    case 'rule_remove': return removeRule(board, action);
    case 'limit': return setLimit(board, action);
    case 'checkpoint': return setCheckpoint(board, action);
    case 'checkin': return setCheckin(board, action);
    default: return assertNever(action);
  }
};

// -- Reserve, acknowledge, and reconcile -------------------------------------
export const reserveRun = (board: Board, taskId: string, entry: OutboxEntry): Board => {
  // The immutable exact-intent operation receipt reserves this identity forever,
  // even after the run has left current state. No growing used-ID set is needed.
  const runId = entry.operationId;
  requirePolicy(entry.payload.kind === 'dispatch' && entry.payload.runId === runId, 'invalid_run_identity');
  const task = taskById(board, taskId);
  noLive(board, task.id);
  requirePolicy(task.status === 'queued' && dependenciesDone(board, task), 'task_not_ready');
  requirePolicy(!board.asks.some(ask => pendingAsk(ask) && ask.blocks.some(id => canonicalTask(board, id).id === task.id)), 'pending_decision');
  requirePolicy(board.concurrencyLimit === null || board.runs.filter(isLive).length < board.concurrencyLimit, 'concurrency_limit');
  const next = replaceTask(board, { ...task, status: 'running', currentRun: runId });
  return { ...next, runs: [...board.runs, { id: runId, taskId, operationId: entry.operationId, status: 'reserved', observedSequence: -1 }], outbox: [...board.outbox, entry] };
};
export const reserveControl = (board: Board, entry: OutboxEntry): Board => {
  requirePolicy(entry.payload.kind === 'control', 'invalid_control');
  if (entry.payload.kind !== 'control') throw new PolicyError('invalid_control');
  const run = runById(board, entry.payload.runId);
  requirePolicy(isLive(run) && run.jobId === entry.payload.jobId && boundedText(entry.payload.text, 12000), 'worker_not_addressable');
  return { ...board, outbox: [...board.outbox, entry] };
};
export const markAttempted = (board: Board, operationId: string, attemptId: string, effect: EffectIdentity): Board => {
  const entry = board.outbox.find(item => item.operationId === operationId);
  requirePolicy(entry?.status === 'reserved', 'already_attempted');
  return { ...board, outbox: board.outbox.map(item => item.operationId === operationId ? { ...item, status: 'attempted', attemptId, effect } : item) };
};
const dispatchReceipt = (board: Board, entry: OutboxEntry, receipt: Extract<EffectReceipt, { kind: 'dispatch' }>): Board => {
  requirePolicy(entry.payload.kind === 'dispatch', 'wrong_receipt_kind');
  if (entry.payload.kind !== 'dispatch') throw new PolicyError('wrong_receipt_kind');
  const run = runById(board, entry.payload.runId);
  const conversation = entry.payload.conversation;
  requirePolicy(validId(receipt.jobId) && validId(receipt.conversationId), 'invalid_job_receipt');
  requirePolicy(conversation.kind === 'fresh' || conversation.conversationId === receipt.conversationId, 'wrong_retained_conversation');
  requirePolicy(!run.jobId || run.jobId === receipt.jobId, 'wrong_job_receipt');
  const next = replaceRun(board, { ...run, jobId: receipt.jobId, conversationId: receipt.conversationId, status: 'queued' });
  return replaceTask(next, { ...taskById(next, run.taskId), conversationId: receipt.conversationId });
};
const rejectedReceipt = (board: Board, entry: OutboxEntry, receipt: Extract<EffectReceipt, { kind: 'rejected' }>): Board => {
  requirePolicy(/^[a-f0-9]{64}$/.test(receipt.requestId) && boundedText(receipt.receiptId, 200)
    && ['network_action_rejected', 'network_action_refused', 'network_rejection_unrepresentable'].includes(receipt.reason), 'invalid_rejection_receipt');
  if (entry.payload.kind !== 'dispatch') return board;
  const run = runById(board, entry.payload.runId);
  const next = replaceRun(board, { ...run, status: 'failed', reason: receipt.reason });
  return replaceTask(next, { ...taskById(next, run.taskId), status: 'blocked', reason: receipt.reason });
};
const controlReceipt = (board: Board, entry: OutboxEntry, receipt: Extract<EffectReceipt, { kind: 'control' }>): Board => {
  requirePolicy(entry.payload.kind === 'control', 'wrong_receipt_kind');
  if (entry.payload.kind !== 'control') throw new PolicyError('wrong_receipt_kind');
  requirePolicy(entry.payload.jobId === receipt.jobId, 'wrong_job_receipt');
  if (receipt.status !== 'applied' || entry.payload.control !== 'steer') return board;
  const run = runById(board, entry.payload.runId);
  // Acknowledgement may arrive after settlement. Retain its receipt/history,
  // but never rewrite an already-settled task contract with a late steer.
  if (!isLive(run)) return board;
  return replaceTask(board, { ...taskById(board, run.taskId), brief: entry.payload.text });
};
const wakeReceipt = (board: Board, entry: OutboxEntry): Board => { requirePolicy(entry.payload.kind === 'wake', 'wrong_receipt_kind'); return board; };
const applyReceipt = (board: Board, entry: OutboxEntry, receipt: EffectReceipt): Board => {
  switch (receipt.kind) {
    case 'dispatch': return dispatchReceipt(board, entry, receipt);
    case 'control': return controlReceipt(board, entry, receipt);
    case 'wake': return wakeReceipt(board, entry);
    case 'rejected': return rejectedReceipt(board, entry, receipt);
    default: return assertNever(receipt);
  }
};
export const acknowledge = (board: Board, receipt: EffectReceipt): Board => {
  const entry = board.outbox.find(item => item.operationId === receipt.operationId);
  if (!entry) throw new PolicyError('unknown_operation');
  requirePolicy(entry.status === 'attempted', 'receipt_without_attempt');
  const next = applyReceipt(board, entry, receipt);
  // The store archives this delta and the receipt before CAS. Acknowledged
  // effects are history, not forever-growing current board items.
  return { ...next, outbox: next.outbox.filter(item => item.operationId !== receipt.operationId) };
};

// -- Provider event transitions (worker claims never accept tasks) ------------
export const validateProgress = (progress: Progress): void => {
  requirePolicy(Number.isSafeInteger(progress.sequence) && progress.sequence >= 0 && boundedText(progress.summary, 2000) && typeof progress.next === 'string' && progress.next.length <= 1200 && validRefs(progress.artifacts), 'invalid_progress');
  requirePolicy(progress.blocker === undefined || boundedText(progress.blocker, 1200), 'invalid_blocker');
  requirePolicy(progress.source === undefined || validRefs([progress.source]), 'invalid_checkpoint_source');
};
export const saveProgress = (board: Board, runId: string, jobId: string, progress: Progress): Board => {
  validateProgress(progress);
  const run = runById(board, runId);
  requirePolicy(run.jobId === jobId, 'wrong_job');
  if (progress.sequence <= run.observedSequence) return board;
  requirePolicy(isLive(run), 'terminal_run');
  const content = ({ sequence: _sequence, source: _source, ...value }: Progress): string => JSON.stringify(value);
  const meaningful = !run.progress || content(run.progress) !== content(progress);
  const nextProgress = meaningful ? progress : { ...run.progress!, source: progress.source };
  return replaceRun(board, { ...run, status: 'running', observedSequence: progress.sequence, progress: nextProgress });
};
export const saveReportClaim = (board: Board, runId: string, jobId: string, report: FileRef): Board => {
  const run = runById(board, runId);
  requirePolicy(run.jobId === jobId && isLive(run) && validRefs([report]), 'invalid_result');
  return replaceRun(board, { ...run, report });
};
export const saveResult = (board: Board, runId: string, jobId: string, status: 'completed' | 'failed' | 'cancelled', report: FileRef): Board => {
  const run = runById(board, runId);
  requirePolicy(run.jobId === jobId && isLive(run) && validRefs([report]), 'invalid_result');
  const next = replaceRun(board, { ...run, status, report });
  const task = taskById(board, run.taskId);
  const succeeded = status === 'completed';
  return replaceTask(next, { ...task, status: succeeded ? 'review' : 'blocked', ...(succeeded ? {} : { reason: `worker_${status}` }) });
};
export const saveDecision = (board: Board, askId: string, decision: NonNullable<Ask['reply']>): Board => {
  const ask = askById(board, askId);
  requirePolicy(ask.status === 'open' && !ask.reply, 'reply_already_saved');
  requirePolicy(ask.addressedTo.includes(decision.source.memberId), 'wrong_member');
  requirePolicy(decision.source.conversationId === board.conversationId, 'wrong_decision_conversation');
  return { ...board, asks: board.asks.map(item => item.id === askId ? { ...item, status: 'replied', reply: decision } : item) };
};

// Retired runs remain in the pinned history chain, not current protected keys.
export const retainCurrentRuns = (board: Board): Board => {
  const taskRuns = board.tasks.flatMap(task => task.currentRun ? [task.currentRun] : []);
  const askRuns = board.asks.filter(pendingAsk).flatMap(ask => ask.sources.map(source => source.runId));
  const pendingRuns = board.outbox.flatMap(entry => {
    switch (entry.payload.kind) {
      case 'dispatch': case 'control': return [entry.payload.runId];
      case 'wake': return [];
    }
  });
  const keep = new Set([...taskRuns, ...askRuns, ...pendingRuns]);
  return { ...board, runs: board.runs.filter(run => isLive(run) || keep.has(run.id)) };
};

// Authoritative absence is distinct from a missing/uncertain dispatch receipt.
export const markUnavailable = (board: Board, runId: string, jobId: string): Board => {
  const run = runById(board, runId);
  requirePolicy(isLive(run) && run.jobId === jobId, 'worker_not_addressable');
  const reason = 'worker_unavailable_inspect_history_before_explicit_requeue';
  const next = replaceRun(board, { ...run, status: 'interrupted', reason });
  return replaceTask(next, { ...taskById(next, run.taskId), status: 'blocked', reason });
};
