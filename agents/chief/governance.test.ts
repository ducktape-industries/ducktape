// Chief governs the whole body of work, not one locally-defensible task at a
// time. These tests drive the durable board and its projections: causation that
// survives relay through a member, growth measured in work that changes the
// repository, and cross-hashing against work already accepted.
import assert from 'node:assert/strict';
import test from 'node:test';
import type { ExtensionAPI, ToolDefinition } from '@earendil-works/pi-coding-agent';

import type { Board, ChiefBridge, DomainAction, Task, TaskSpec } from './contracts.ts';
import { census, decide, emptyBoard, lineRoot, linesOfWork, relatedWork, scopeDrift, validateBoard, LINE_LADDER } from './domain.ts';
import type { LineOfWork } from './domain.ts';
import { registerChief } from './index.ts';
import { CHIEF_PROMPT } from './prompt.ts';
import { createChiefService } from './service.ts';
import { conversationId, memory, ref } from './test-support.ts';
import { boardView, lineNotices, provenanceNotices, VIEW_CHARACTERS } from './views.ts';

// -- Board fixtures ----------------------------------------------------------
const spec = (id: string, overrides: Partial<TaskSpec> = {}): TaskSpec =>
  ({ id, key: id, title: `Task ${id}`, brief: `Implement ${id}`, scope: [id], access: 'write', dependencies: [], ...overrides });
const put = (board: Board, id: string, overrides: Partial<TaskSpec> = {}): Board =>
  decide(board, { kind: 'task_put', task: spec(id, overrides) });
const task = (board: Board, id: string): Task => board.tasks.find(item => item.id === id)!;
const line = (board: Board, rootId: string): LineOfWork => linesOfWork(board).find(entry => entry.rootId === rootId)!;
const reloaded = (board: Board): Board => validateBoard(JSON.parse(JSON.stringify(board)), conversationId);
const boardOverview = (board: Board): Record<string, unknown> => boardView(board, { kind: 'board', section: 'overview', offset: 0, limit: 25 });
// The cached role prefix as Pi assembles it: the static doctrine, the tool
// definitions and the active tool list, with nothing board-derived in any of them.
const capturePrefix = (bridge: ChiefBridge) => {
  const tools = new Map<string, ToolDefinition>();
  const handlers = new Map<string, (event: Record<string, unknown>) => unknown>();
  const active: string[][] = [];
  const api = {
    registerTool: (tool: ToolDefinition) => { tools.set(tool.name, tool); },
    on: (event: string, handler: (input: Record<string, unknown>) => unknown) => { handlers.set(event, handler); },
    setActiveTools: (names: string[]) => { active.push(names); },
    sendMessage: () => undefined,
    events: { emit: () => undefined, on: () => () => undefined },
  } as unknown as ExtensionAPI;
  registerChief(api, bridge);
  const started = handlers.get('before_agent_start')!({ systemPrompt: 'native system prompt' }) as { systemPrompt: string };
  return { systemPrompt: started.systemPrompt, activeTools: JSON.stringify(active.at(-1)),
    tools: JSON.stringify([...tools.values()].map(tool => ({ name: tool.name, description: tool.description, promptSnippet: tool.promptSnippet, parameters: tool.parameters }))) };
};

// -- Durable fixtures: the same commands the model issues --------------------
const chief = () => {
  const adapters = memory();
  return { adapters, service: createChiefService(adapters, conversationId), board: () => adapters.state.board };
};
type Chief = ReturnType<typeof chief>;
const apply = async (ctx: Chief, operationId: string, action: DomainAction) => {
  const result = await ctx.service.execute({ kind: 'change', operationId, expectedRevision: ctx.board().revision, action });
  assert.equal(result.success, true, JSON.stringify(result));
  return result.success ? result.data : {};
};
// One full run of a task: dispatched, settled by the host, accepted by Chief
// against reviewed evidence, recording the paths that run actually changed.
const settle = async (ctx: Chief, id: string, attempt: string, footprint?: string[]) => {
  const runId = `run-${attempt}`;
  const dispatched = await ctx.service.execute({ kind: 'dispatch', operationId: runId, expectedRevision: ctx.board().revision, taskId: id, fresh: false });
  assert.equal(dispatched.success, true, JSON.stringify(dispatched));
  const settled = await ctx.service.receive({ kind: 'result', operationId: `result-${attempt}`, runId, jobId: `job:${runId}`, status: 'completed', report: `Raw claim from ${runId}` });
  assert.equal(settled.success, true, JSON.stringify(settled));
  return apply(ctx, `accept-${attempt}`, { kind: 'accept', taskId: id, outcome: `Reviewed ${id}`, evidence: [ref], footprint });
};

// -- Causation ---------------------------------------------------------------
test('origin records added work, is distinct from ordering, and survives reload', () => {
  const root = put(emptyBoard(conversationId), 'contract');
  // The defect the contract suite found: caused by that task, ordered by nothing.
  const child = put(root, 'defect', { origin: 'contract' });
  assert.equal(task(child, 'defect').origin, 'contract');
  assert.deepEqual(task(child, 'defect').dependencies, []);
  assert.equal(lineRoot(child, 'defect').id, 'contract');
  assert.deepEqual(reloaded(child), child);
  // A line is the origin tree, not the dependency graph: an ordered task a
  // member asked for independently is its own line however much it waits.
  const ordered = put(child, 'unrelated', { dependencies: ['contract'] });
  assert.equal(lineRoot(ordered, 'unrelated').id, 'unrelated');
  assert.equal(linesOfWork(ordered).length, 2);
});

test('causation survives relay through the member and stays out of the worker brief', async () => {
  const ctx = chief();
  await apply(ctx, 'found', { kind: 'task_put', task: spec('audit', { access: 'read', scope: ['crates/airlock'] }) });
  // A member relays a finding Chief surfaced. Recording that as member-initiated
  // work is how growth hides, so the fix still descends from the task that found it.
  await apply(ctx, 'relayed', { kind: 'task_put', task: spec('airlock-fix', { origin: 'audit', scope: ['crates/airlock'] }) });
  assert.equal(lineRoot(ctx.board(), 'airlock-fix').id, 'audit');
  assert.equal(line(ctx.board(), 'audit').tasks, 2);
  const dispatched = await ctx.service.execute({ kind: 'dispatch', operationId: 'run-fix', expectedRevision: ctx.board().revision, taskId: 'airlock-fix', fresh: true });
  assert.equal(dispatched.success, true, JSON.stringify(dispatched));
  const brief = ctx.adapters.submissions[0].payload;
  assert.equal(brief.kind, 'dispatch');
  assert(brief.kind === 'dispatch');
  assert.equal(brief.prompt.includes('audit'), false, 'no worker brief carries provenance');
  assert.match(brief.prompt, /git diff --name-only/);
  assert.match(brief.prompt, /ducktape_report_job checkpoint whose blocker/);
});

test('origin cannot point at itself, close a cycle, or be forged through persistence', async () => {
  const board = put(put(emptyBoard(conversationId), 'a'), 'b');
  assert.throws(() => reloaded(put(board, 'a', { origin: 'a' })), /invalid_origin/);
  assert.throws(() => reloaded(put(board, 'a', { origin: 'missing-task' })), /unknown_task/);
  const chained = put(board, 'b', { origin: 'a' });
  // In memory: the durable write refuses before anything is committed.
  assert.throws(() => reloaded(put(chained, 'a', { origin: 'b' })), /origin_cycle/);
  const ctx = chief();
  await apply(ctx, 'a', { kind: 'task_put', task: spec('a') });
  await apply(ctx, 'b', { kind: 'task_put', task: spec('b', { origin: 'a' }) });
  const refused = await ctx.service.execute({ kind: 'change', operationId: 'cycle', expectedRevision: ctx.board().revision, action: { kind: 'task_put', task: spec('a', { origin: 'b' }) } });
  assert.deepEqual(refused, { success: false, error: 'origin_cycle' });
  // On reload: a hand-edited cycle fails closed rather than hanging every
  // projection that walks the line.
  const forged = JSON.parse(JSON.stringify(chained)) as Board;
  forged.tasks.find(item => item.id === 'a')!.origin = 'b';
  assert.throws(() => validateBoard(forged, conversationId), /origin_cycle/);
});

test('provenance is correctable without reopening accepted work or stopping a live worker', async () => {
  const ctx = chief();
  await apply(ctx, 'root', { kind: 'task_put', task: spec('root') });
  await apply(ctx, 'follow', { kind: 'task_put', task: spec('follow') });
  const dispatched = await ctx.service.execute({ kind: 'dispatch', operationId: 'run-follow', expectedRevision: ctx.board().revision, taskId: 'follow', fresh: false });
  assert.equal(dispatched.success, true, JSON.stringify(dispatched));
  // Chief realises mid-flight that the sibling, not the member, caused this task.
  await apply(ctx, 'correct', { kind: 'task_put', task: spec('follow', { origin: 'root' }) });
  assert.equal(task(ctx.board(), 'follow').origin, 'root');
  // The worker's brief never carries origin, so nothing it is executing changed.
  const steered = await ctx.service.execute({ kind: 'change', operationId: 'respec', expectedRevision: ctx.board().revision, action: { kind: 'task_put', task: spec('follow', { origin: 'root', brief: 'different' }) } });
  assert.deepEqual(steered, { success: false, error: 'live_worker' });
  // The same holds for work already accepted: recording what caused it is
  // bookkeeping, and must not require reopening a verified outcome.
  const done = chief();
  await apply(done, 'cause', { kind: 'task_put', task: spec('cause') });
  await apply(done, 'shipped', { kind: 'task_put', task: spec('shipped') });
  await settle(done, 'shipped', 'shipped');
  await apply(done, 'late', { kind: 'task_put', task: spec('shipped', { origin: 'cause' }) });
  assert.equal(task(done.board(), 'shipped').origin, 'cause');
  assert.equal(task(done.board(), 'shipped').status, 'done');
  assert.equal(task(done.board(), 'shipped').acceptance?.outcome, 'Reviewed shipped');
  const rewritten = await done.service.execute({ kind: 'change', operationId: 'rewrite', expectedRevision: done.board().revision, action: { kind: 'task_put', task: spec('shipped', { origin: 'cause', brief: 'rewritten' }) } });
  assert.deepEqual(rewritten, { success: false, error: 'reopen_before_edit' });
});

test('merging tasks keeps the line whole and never leaves a task originating from itself', () => {
  const board = ['root', 'source', 'child'].reduce((state, id) => put(state, id), emptyBoard(conversationId));
  const origins = put(put(board, 'source', { origin: 'root' }), 'child', { origin: 'source' });
  const merged = decide(origins, { kind: 'task_merge', sourceId: 'source', targetId: 'child' });
  // The child absorbed its own origin: it must not originate from itself, and
  // the surviving task keeps the root so the line does not fragment.
  assert.equal(task(merged, 'child').origin, 'root');
  assert.equal(lineRoot(merged, 'child').id, 'root');
  assert.equal(line(merged, 'root').tasks, 2);
  assert.deepEqual(reloaded(merged), merged);
  // A dependent standing on the absorbed task follows it to the survivor.
  const dependent = put(origins, 'later', { origin: 'source' });
  const followed = decide(dependent, { kind: 'task_merge', sourceId: 'source', targetId: 'child' });
  assert.equal(task(followed, 'later').origin, 'child');
  assert.equal(lineRoot(followed, 'later').id, 'root');
});

// -- Measurement -------------------------------------------------------------
test('footprint accumulates across runs and measures declared scope against reality', async () => {
  const ctx = chief();
  await apply(ctx, 'ops', { kind: 'task_put', task: spec('ops', { scope: ['ops'] }) });
  await settle(ctx, 'ops', 'first', ['ops/deploy.sh', 'crates/generated/wire.rs']);
  assert.deepEqual(task(ctx.board(), 'ops').footprint, ['ops/deploy.sh', 'crates/generated/wire.rs']);
  assert.deepEqual(scopeDrift(task(ctx.board(), 'ops')), ['crates/generated/wire.rs']);
  // Reopening and running again adds to the record; it never shrinks it.
  await apply(ctx, 'requeue', { kind: 'task_status', taskId: 'ops', status: 'queued', reason: 'One more pass' });
  await settle(ctx, 'ops', 'second', ['ops/deploy.sh', 'app/main.ts']);
  assert.deepEqual(task(ctx.board(), 'ops').footprint, ['ops/deploy.sh', 'crates/generated/wire.rs', 'app/main.ts']);
  assert.deepEqual(scopeDrift(task(ctx.board(), 'ops')), ['crates/generated/wire.rs', 'app/main.ts']);
  assert.deepEqual(reloaded(ctx.board()), ctx.board());
  // A whole-repo declaration predicts everything, so nothing can drift from it.
  await apply(ctx, 'wide', { kind: 'task_put', task: spec('wide', { scope: ['.'] }) });
  await settle(ctx, 'wide', 'wide', ['anything/at/all.ts']);
  assert.deepEqual(scopeDrift(task(ctx.board(), 'wide')), []);
  // Chief reads the footprint out of reviewed evidence; nothing else may write
  // it, and a glob or a traversal is not a path a run changed.
  const blocked = await ctx.service.execute({ kind: 'change', operationId: 'forged', expectedRevision: ctx.board().revision,
    action: { kind: 'task_status', taskId: 'wide', status: 'blocked', reason: 'stop', footprint: ['../outside.ts'] } });
  assert.deepEqual(blocked, { success: false, error: 'invalid_footprint' });
  const globbed = await ctx.service.execute({ kind: 'change', operationId: 'globbed', expectedRevision: ctx.board().revision,
    action: { kind: 'task_status', taskId: 'wide', status: 'blocked', reason: 'stop', footprint: ['crates/**/*.rs'] } });
  assert.deepEqual(globbed, { success: false, error: 'invalid_footprint' });
  // The record a task grows is bounded: one task cannot carry an unbounded
  // footprint into a single durable record.
  const oversize = await ctx.service.execute({ kind: 'change', operationId: 'oversize', expectedRevision: ctx.board().revision,
    action: { kind: 'task_status', taskId: 'wide', status: 'blocked', reason: 'stop', footprint: Array.from({ length: 200 }, (_, index) => `${'deep'.repeat(20)}/file-${index}.rs`) } });
  assert.deepEqual(oversize, { success: false, error: 'invalid_footprint' });
});

test('a line is measured by work that changes the repository, not by task count', () => {
  const board = put(emptyBoard(conversationId), 'release');
  // Verification growth is growth a chief should defend: six read-only children.
  const researched = Array.from({ length: 6 }, (_, index) => `check${index}`)
    .reduce((state, id) => put(state, id, { origin: 'release', access: 'read' }), board);
  assert.equal(line(researched, 'release').tasks, 7);
  assert.equal(line(researched, 'release').changing, 1);
  const notices = lineNotices(researched, ['check5'], board);
  assert.equal(notices.length, 1);
  assert.equal(JSON.stringify(notices).includes('crossedRung'), false);
  assert.match(JSON.stringify(notices), /judge the whole line/);
});

test('a rung is crossed once by changing work and puts the aggregate to a ruling', () => {
  const start = put(emptyBoard(conversationId), 'release');
  const grow = (board: Board, id: string): Board => put(board, id, { origin: 'release' });
  const second = grow(start, 'fix1');
  const third = grow(second, 'fix2');
  // Two changing tasks is below the first rung; the third crosses it.
  assert.equal(JSON.stringify(lineNotices(second, ['fix1'], start)).includes('crossedRung'), false);
  const crossing = lineNotices(third, ['fix2'], second);
  assert.equal(JSON.stringify(crossing).includes(`"crossedRung":${LINE_LADDER[0]}`), true);
  assert.match(JSON.stringify(crossing), /RULING POINT/);
  // The same line reported again without having grown must not re-fire: a rung
  // is a ruling point, not a standing nag that trains Chief to scroll past it.
  assert.equal(JSON.stringify(lineNotices(third, ['fix2'], third)).includes('crossedRung'), false);
  // A caller with no before-state reports the standing size and fires nothing.
  assert.equal(JSON.stringify(lineNotices(third, ['fix2'])).includes('crossedRung'), false);
  const fourth = grow(third, 'fix3');
  assert.equal(JSON.stringify(lineNotices(fourth, ['fix3'], third)).includes('crossedRung'), false);
  const fifth = grow(fourth, 'fix4');
  assert.equal(JSON.stringify(lineNotices(fifth, ['fix4'], fourth)).includes(`"crossedRung":${LINE_LADDER[1]}`), true);
});

test('delivery of work already ruled on reaches no new ground, and rungs still fire regardless', () => {
  // The shape from a real release: four tasks that author, three that deliver
  // or verify work already accepted. All seven carry write access, because
  // delivery touches the repo or a live target too.
  const start = put(emptyBoard(conversationId), 'release', { scope: ['contract'] });
  const authoring = [['guard', 'crates/guard'], ['assertion', 'crates/assertion'], ['ci-repair', '.github/workflows']]
    .reduce((board, [id, scope]) => put(board, id, { origin: 'release', scope: [scope] }), start);
  assert.equal(line(authoring, 'release').changing, 4);
  assert.equal(line(authoring, 'release').newGround, 4);
  // Delivery declares only ground the line already owns: merging the guards,
  // promoting the release, driving the canary against what was already built.
  const delivered = [['merge-guards', 'crates/guard'], ['promote', 'contract'], ['canary', 'crates/assertion']]
    .reduce((board, [id, scope]) => put(board, id, { origin: 'release', scope: [scope] }), authoring);
  assert.equal(line(delivered, 'release').changing, 7);
  assert.equal(line(delivered, 'release').newGround, 4, 'delivery must not inflate the count that measures growth');
  // The guarantee survives the discount: a rung still fires on changing work,
  // so a line can never grow silently just because it reuses its own ground.
  const notices = JSON.stringify(lineNotices(delivered, ['canary'], authoring));
  assert.match(notices, /RULING POINT/);
  assert.match(notices, /4 of which reached ground/);
  // Read-only verification never counts toward either number.
  const verified = put(delivered, 'review-range', { origin: 'release', access: 'read', scope: ['everywhere'] });
  assert.equal(line(verified, 'release').changing, 7);
  assert.equal(line(verified, 'release').newGround, 4);
});

test('origin follows the discovery chain, not the authorship of the defect', async () => {
  // An old task wrote the code; a later review found consent defects in it.
  const ctx = chief();
  await apply(ctx, 'authored', { kind: 'task_put', task: spec('wrote-consent-code', { scope: ['crates/consent'] }) });
  await settle(ctx, 'wrote-consent-code', 'authored', ['crates/consent/flow.rs']);
  await apply(ctx, 'review', { kind: 'task_put', task: spec('independent-review', { access: 'read', scope: ['crates'] }) });
  // The fix descends from the review that surfaced it. The line that grew is
  // the review's, and the authoring task's accepted outcome is untouched by it.
  const fixed = put(ctx.board(), 'consent-fix', { origin: 'independent-review', scope: ['crates/consent'] });
  assert.equal(lineRoot(fixed, 'consent-fix').id, 'independent-review');
  assert.equal(line(fixed, 'independent-review').tasks, 2);
  assert.equal(line(fixed, 'wrote-consent-code').tasks, 1);
  assert.equal(task(fixed, 'wrote-consent-code').acceptance?.outcome, 'Reviewed wrote-consent-code');
  // Attributing to the author instead would hide that growth inside a task
  // that was already accepted and finished.
  const misattributed = put(ctx.board(), 'consent-fix-wrong', { origin: 'wrote-consent-code', scope: ['crates/consent'] });
  assert.equal(line(misattributed, 'independent-review').tasks, 1);
});

// -- Cross-hashing -----------------------------------------------------------
test('work added onto accepted work is flagged without any threshold', async () => {
  const ctx = chief();
  await apply(ctx, 'ship', { kind: 'task_put', task: spec('shipped', { scope: ['crates/wire'] }) });
  await settle(ctx, 'shipped', 'shipped', ['crates/wire/frame.rs']);
  const before = ctx.board();
  const added = await apply(ctx, 'regress', { kind: 'task_put', task: spec('regression', { origin: 'shipped', scope: ['crates/wire'] }) });
  assert.match(JSON.stringify(added.reopensAcceptedWork), /already accepted as finished/);
  assert.equal(provenanceNotices(ctx.board(), ['regression'], before).length, 1);
  // It fires at the change, once: repeating the same acknowledgment must not.
  assert.deepEqual(provenanceNotices(ctx.board(), ['regression'], ctx.board()), []);
  // Work descending from work still in flight is ordinary growth, not a reopening.
  const live = put(ctx.board(), 'ordinary', { origin: 'regression' });
  assert.deepEqual(provenanceNotices(live, ['ordinary'], ctx.board()), []);
});

test('tasks are cross-hashed by shared surface, including against accepted outcomes', async () => {
  const ctx = chief();
  await apply(ctx, 'auth', { kind: 'task_put', task: spec('auth', { scope: ['crates/auth'] }) });
  await settle(ctx, 'auth', 'auth', ['crates/auth/token.rs']);
  await apply(ctx, 'ui', { kind: 'task_put', task: spec('ui', { scope: ['app/ui'] }) });
  // A new task declaring somewhere else entirely, whose actual work landed on
  // the accepted task's ground, is still related — footprint counts, not claims.
  await apply(ctx, 'report', { kind: 'task_put', task: spec('report', { scope: ['docs'] }) });
  await settle(ctx, 'report', 'report', ['crates/auth/session.rs']);
  const related = relatedWork(ctx.board(), ['report']);
  assert.deepEqual(related.find(entry => entry.surface === 'crates')!.accepted.map(item => item.id), ['auth']);
  assert.equal(related.find(entry => entry.surface === 'docs'), undefined);
  // Cancelled work is not live ground; it must not be reconciled against.
  await apply(ctx, 'abandon-put', { kind: 'task_put', task: spec('abandoned', { scope: ['crates/auth'] }) });
  assert.deepEqual(relatedWork(ctx.board(), ['report']).find(entry => entry.surface === 'crates')!.live.map(item => item.id), ['abandoned']);
  await apply(ctx, 'abandon', { kind: 'task_status', taskId: 'abandoned', status: 'cancelled', reason: 'Superseded by the auth work' });
  assert.deepEqual(relatedWork(ctx.board(), ['report']).flatMap(entry => entry.live), []);
});

test('the census counts every task even when the prioritized preview cannot show it', () => {
  const many = Array.from({ length: 120 }, (_, index) => index).reduce((board, index) =>
    put(board, `task-${index}`, { scope: [`surface-${index % 4}/module`], access: index % 3 ? 'write' : 'read' }), emptyBoard(conversationId));
  const view = boardOverview(validateBoard(many, conversationId));
  const totals = view.census as ReturnType<typeof census> & { distinctSurfaces: number };
  // The preview drops work by design; the census is the thing that cannot.
  assert.equal((view.tasks as unknown[]).length < 120, true);
  assert.equal(totals.tasks, 120);
  assert.equal(totals.byStatus.queued, 120);
  assert.equal(totals.distinctSurfaces, 4);
  assert.match(String(view.provenance), /census counts EVERY task and surface/);
  assert.equal(JSON.stringify(view).length <= VIEW_CHARACTERS, true);
});

// -- Delivery ----------------------------------------------------------------
test('the governing view rides on the mutation acknowledgment a change already returns', async () => {
  const ctx = chief();
  await apply(ctx, 'release', { kind: 'task_put', task: spec('release', { scope: ['crates'] }) });
  await apply(ctx, 'fix1', { kind: 'task_put', task: spec('fix1', { origin: 'release', scope: ['crates'] }) });
  const data = await apply(ctx, 'fix2', { kind: 'task_put', task: spec('fix2', { origin: 'release', scope: ['crates'] }) });
  assert.match(JSON.stringify(data.linesOfWork), /crossedRung/);
  assert.match(JSON.stringify(data.sharedSurfaces), /"surface":"crates"/);
  assert.equal(JSON.stringify(data).length <= 2500, true, `Acknowledgment stayed at ${JSON.stringify(data).length} characters`);
  // A replay has no before-state of its own: it must not re-fire the rung its
  // first commit already ruled on.
  const replay = await ctx.service.execute({ kind: 'change', operationId: 'fix2', expectedRevision: ctx.board().revision - 1, action: { kind: 'task_put', task: spec('fix2', { origin: 'release', scope: ['crates'] }) } });
  assert.equal(replay.success, true, JSON.stringify(replay));
  assert.equal(JSON.stringify(replay.success && replay.data).includes('crossedRung'), false);
  // A single-task line has no whole to rule on and must stay silent.
  const lone = chief();
  const first = await apply(lone, 'solo', { kind: 'task_put', task: spec('solo') });
  assert.deepEqual(Object.keys(first).toSorted(), ['operationId', 'replayed', 'revision']);
});

test('the cached prefix is byte-identical as the board grows; the governing facts ride on results', async () => {
  const ctx = chief();
  const before = capturePrefix(ctx.service);
  await apply(ctx, 'release', { kind: 'task_put', task: spec('release', { scope: ['crates/release'] }) });
  const grown = await ['fix1', 'fix2', 'fix3'].reduce((previous, id) => previous
    .then(seen => apply(ctx, id, { kind: 'task_put', task: spec(id, { origin: 'release', scope: [`crates/${id}`] }) })
      .then(data => [...seen, data])), Promise.resolve([] as Record<string, unknown>[]));
  const after = capturePrefix(ctx.service);
  assert.equal(after.systemPrompt, before.systemPrompt);
  assert.equal(after.tools, before.tools);
  assert.equal(after.activeTools, before.activeTools);
  assert.equal(before.systemPrompt.includes(CHIEF_PROMPT), true);
  assert.equal(before.tools.includes('release'), false, 'no board state reaches a tool definition');
  // The prefix never moved, and the governing facts were delivered anyway.
  assert.match(JSON.stringify(grown.map(data => data.linesOfWork)), /"crossedRung":3/);
  const overview = await ctx.service.execute({ kind: 'board', section: 'overview', offset: 0, limit: 5 });
  assert.equal(overview.success, true, JSON.stringify(overview));
  assert.equal(overview.success && (overview.data.census as { tasks: number }).tasks, 4);
  assert.match(JSON.stringify(overview.success && overview.data.linesOfWork), /"newGround":4/);
});
