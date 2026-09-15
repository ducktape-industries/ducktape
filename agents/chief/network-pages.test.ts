// The native wire fake keeps document rows, protected keys and immutable
// receipt-owned roots separate. It exposes only existing RecordState(key), not
// an invented enumeration route or a whole-board persistence shortcut.
import { strict as assert } from 'node:assert';
import { createHash } from 'node:crypto';
import { test } from 'node:test';

import type { Board, FileRef, OperationMetadata, PagesAdapter, Task } from './contracts.ts';
import { emptyBoard } from './domain.ts';
import { createNetworkPages, managedRecordPageId, managedRecordTargetMatches } from './network-pages.ts';
import type { NetworkPagesRpc } from './network-pages.ts';
import { createChiefService } from './service.ts';
import { memory } from './test-support.ts';

// -- Exact current Pages wire ------------------------------------------------
interface DocumentRow { record_id: string; data: { kind: string; value: Record<string, unknown> }; revision: number }
interface StateRow { key: string; value: Record<string, unknown>; revision: number }
interface Receipt { page_id: string; request_id: string; revision: number; payload_digest: number[]; metadata: OperationMetadata; artifacts: string[] }
interface Commit {
  page_id: string; request_id: string; expected_revision: number; metadata: OperationMetadata; artifacts: string[];
  changes: ({ delete: { record_id: string } } | { upsert: {
    record_id: string; data: DocumentRow['data'];
    document: { title: string; blocks: { id: string; kind: string; text: string; marks: unknown[] }[] };
  } })[];
  state_changes: ({ delete: { key: string } } | { put: { key: string; value: Record<string, unknown> } })[];
}
interface MockState {
  revision: number; documents: Map<string, DocumentRow>; protected: Map<string, StateRow>; receipts: Map<string, Receipt>;
  retained: Map<string, string[]>; submits: Commit[]; queries: Record<string, unknown>[];
  tamperReceipt?: (receipt: Receipt) => Receipt;
  onQuery?: (input: Record<string, unknown>) => void;
  rejectArtifacts: boolean; loseReply: boolean;
}
const hash = (value: string): string => createHash('sha256').update(value).digest('hex');
const reference = (name: string): FileRef => ({ fileId: `file-${hash(name).slice(0, 20)}`, hash: hash(name) });
const canonical = (value: unknown): string => {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  const isObject = value !== null && typeof value === 'object';
  if (!isObject) return JSON.stringify(value);
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record).filter(key => record[key] !== undefined)
    .sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)))
    .map(key => `${JSON.stringify(key)}:${canonical(record[key])}`).join(',')}}`;
};
const task = (id: string): Task => ({ id, key: id, title: `Title ${id}`, brief: 'Authorized brief', scope: ['src'], access: 'read', dependencies: [], status: 'queued', evidence: [] });
const createMock = () => {
  const state: MockState = { revision: 0, documents: new Map(), protected: new Map(), receipts: new Map(), retained: new Map(),
    submits: [], queries: [], rejectArtifacts: false, loseReply: false };
  const rpc: NetworkPagesRpc = {
    query: async (module, input, signal) => {
      signal?.throwIfAborted();
      assert.equal(module, 'pages');
      assert.equal(Object.keys(input).length, 1);
      state.queries.push(input);
      state.onQuery?.(input);
      if (input.record_collection) return { record_collection: { page_id: 'board', writer: { account: 7 }, revision: state.revision, record_count: state.documents.size } };
      if (input.record_state) {
        const { key } = input.record_state as { key: string };
        return { record_state: structuredClone(state.protected.get(key) ?? null) };
      }
      if (input.record_receipt) {
        const request = input.record_receipt as { request_id: string };
        const receipt = state.receipts.get(request.request_id) ?? null;
        return { record_receipt: state.tamperReceipt && receipt ? state.tamperReceipt(structuredClone(receipt)) : structuredClone(receipt) };
      }
      assert(input.records, `Unsupported query ${Object.keys(input)[0]}`);
      const { after, limit } = input.records as { after: string | null; limit: number };
      assert.equal(limit, 32);
      const rows = [...state.documents.values()].sort((a, b) => Buffer.compare(Buffer.from(a.record_id), Buffer.from(b.record_id)))
        .filter(row => after === null || row.record_id > after);
      const records = rows.slice(0, limit);
      return { records: { revision: state.revision, records: structuredClone(records), next_after: rows.length > limit ? records.at(-1)!.record_id : null } };
    },
    submit: async (module, input, requestId, signal) => {
      signal?.throwIfAborted();
      assert.equal(module, 'pages');
      const payload = input.commit_records as Commit;
      assert(payload);
      assert.equal(payload.request_id, requestId);
      assert(Buffer.byteLength(requestId) <= 128);
      assert.equal(payload.expected_revision, state.revision);
      assert(payload.changes.length + payload.state_changes.length <= 16);
      assert(Buffer.byteLength(canonical(input)) <= 128 * 1024);
      assert(Buffer.byteLength(JSON.stringify(payload.metadata)) <= 8 * 1024);
      assert(payload.artifacts.length <= 8 && payload.artifacts.every(root => /^[a-f0-9]{64}$/.test(root)));
      assert(payload.artifacts.includes(payload.metadata.history.hash));
      if (state.rejectArtifacts) throw new Error('native_artifact_retention_refused');
      const revision = state.revision + 1;
      const documents = new Map(state.documents);
      const protectedState = new Map(state.protected);
      payload.changes.forEach(change => {
        if ('delete' in change) { assert(documents.delete(change.delete.record_id)); return; }
        const { record_id, data, document } = change.upsert;
        assert(['task', 'ask', 'rule'].includes(data.kind), 'Operational state must not create a document');
        assert(Buffer.byteLength(JSON.stringify(data)) <= 32 * 1024);
        assert.deepEqual(Object.keys(data).sort(), ['kind', 'value']);
        assert(document.blocks.every(block => block.kind === 'paragraph' && block.marks.length === 0));
        assert(document.blocks.every(block => block.id !== record_id && Buffer.byteLength(block.id) <= 128));
        documents.set(record_id, { record_id, data: structuredClone(data), revision });
      });
      payload.state_changes.forEach(change => {
        if ('delete' in change) { assert(protectedState.delete(change.delete.key)); return; }
        assert(Buffer.byteLength(change.put.key) <= 128);
        assert(Buffer.byteLength(JSON.stringify(change.put.value)) <= 16 * 1024);
        protectedState.set(change.put.key, { ...structuredClone(change.put), revision });
      });
      assert(documents.size <= 1024 && protectedState.size <= 256);
      const receipt: Receipt = { page_id: 'board', request_id: requestId, revision, metadata: structuredClone(payload.metadata),
        payload_digest: [...createHash('sha256').update(canonical(input)).digest()], artifacts: [...payload.artifacts] };
      state.documents = documents;
      state.protected = protectedState;
      state.revision = revision;
      state.receipts.set(requestId, receipt);
      state.retained.set(requestId, [...payload.artifacts]);
      state.submits.push(structuredClone(payload));
      if (state.loseReply) { state.loseReply = false; throw new Error('lost_transport_reply'); }
      return { admitted: true };
    },
  };
  return { state, adapter: createNetworkPages(rpc, 'board', 'conversation') };
};
const readBoard = (adapter: PagesAdapter): Promise<Board> => Promise.resolve().then(() => adapter.read()).then(snapshot => snapshot.value as Board);
const inputFor = (before: Board, id: string, patch: Partial<Board> = {}, artifacts: FileRef[] = [], result?: Record<string, unknown>): Parameters<PagesAdapter['commit']>[0] => {
  const history = reference(`history:${id}`);
  return { expectedRevision: before.revision, requestId: id, value: { ...before, ...patch, revision: before.revision + 1, history },
    metadata: { operationId: id, fingerprint: hash(id), history, ...(result ? { result } : {}) },
    artifacts: [history, ...(before.history ? [before.history] : []), ...artifacts] };
};
const running = (prompt: string): Partial<Board> => ({
  tasks: [{ ...task('task1'), status: 'running', currentRun: 'run1' }],
  runs: [{ id: 'run1', taskId: 'task1', operationId: 'run1', status: 'reserved', observedSequence: -1 }],
  outbox: [{ operationId: 'run1', status: 'reserved', payload: { kind: 'dispatch', runId: 'run1', taskId: 'task1', conversation: { kind: 'fresh' }, prompt } }],
});
const chunk = (state: MockState, kind: string, part = 0): StateRow => {
  const found = [...state.protected.values()].find(row => row.value.kind === kind && row.value.part === part);
  assert(found, `Missing ${kind} part ${part}`);
  return found;
};

// -- Human documents, protected chunks, immutable receipt roots --------------
test('only human entities become documents; state diff and header revision share one CAS', async () => {
  const { state, adapter } = createMock();
  assert.deepEqual(await adapter.read(), { revision: 0, value: emptyBoard('conversation') });
  const id = 'o'.repeat(160);
  const initial = inputFor(emptyBoard('conversation'), id, { tasks: [task('task1')] });
  assert.deepEqual(await adapter.commit(initial), { kind: 'committed', requestId: id, revision: 1 });
  assert.equal(state.documents.size, 1);
  assert.equal(state.protected.size, 3);
  assert.equal(state.submits[0].changes.length, 1);
  assert.equal(state.submits[0].state_changes.length, 3);
  assert.deepEqual(await readBoard(adapter), initial.value);
  assert(!('operations' in (await readBoard(adapter))));
  const page = [...state.documents.values()][0].record_id;
  assert.equal(managedRecordPageId('board', 'task', 'task1'), page);
  assert(managedRecordTargetMatches('board', 'task', 'task1', page));
  assert(managedRecordTargetMatches('board', 'task', 'task1', `${page}-body`));
  assert(!managedRecordTargetMatches('board', 'ask', 'task1', page));
  assert(!managedRecordTargetMatches('other', 'task', 'task1', page));
  assert.deepEqual(await adapter.commit(initial), { kind: 'conflict', revision: 1 });
  assert.equal(state.submits.length, 1);
  const update = inputFor(await readBoard(adapter), 'update', { tasks: [{ ...task('task1'), brief: 'Revised contract' }] });
  await adapter.commit(update);
  assert.equal(state.submits.at(-1)!.changes.length, 1);
  assert.equal(state.submits.at(-1)!.state_changes.length, 2, 'Unchanged manifest chunk must not rewrite');
  assert.equal(state.protected.get('chief-index')!.value.revision, 2);
  assert.equal(state.protected.get('chief-index')!.revision, 2);
  const remove = inputFor(await readBoard(adapter), 'remove', { tasks: [] });
  await adapter.commit(remove);
  assert.equal(state.documents.size, 0);
  assert.deepEqual(await readBoard(adapter), remove.value);
});

test('commit snapshots caller-owned input before asynchronous enumeration', async () => {
  const { state, adapter } = createMock();
  const input = inputFor(emptyBoard('conversation'), 'stable', { tasks: [task('task1')] });
  const expected = structuredClone(input);
  state.onQuery = query => {
    if (!query.record_collection) return;
    input.value.tasks[0].brief = 'Changed after the operation started';
    input.metadata.history = reference('different-history');
    input.artifacts.length = 0;
  };
  assert.equal((await adapter.commit(input)).kind, 'committed');
  state.onQuery = undefined;
  assert.deepEqual(await readBoard(adapter), expected.value);
  assert.deepEqual(await adapter.receipt('stable'), { ...expected.metadata, revision: 1 });
});

test('large UTF-8 runtime entities use complete bounded chunks, shrink cleanly and disappear on acknowledgement', async () => {
  const { state, adapter } = createMock();
  const prompt = '中'.repeat(10000) + '😀'.repeat(5000);
  const initial = inputFor(emptyBoard('conversation'), 'large', running(prompt));
  await adapter.commit(initial);
  assert(state.protected.size > 6);
  assert.equal(state.documents.size, 1);
  assert.equal((await readBoard(adapter)).outbox[0].payload.kind, 'dispatch');
  assert.deepEqual((await readBoard(adapter)).outbox, initial.value.outbox);
  const parts = [...state.protected.values()].filter(row => row.value.kind === 'outbox');
  assert(parts.length > 2);
  assert(parts.every(row => Buffer.byteLength(JSON.stringify(row.value)) <= 16 * 1024));
  await adapter.commit(inputFor(await readBoard(adapter), 'shrink', running('Short replacement')));
  assert.equal([...state.protected.values()].filter(row => row.value.kind === 'outbox').length, 1);
  const effectReceipt = { kind: 'dispatch', operationId: 'run1', jobId: 'job1', conversationId: 'worker1' };
  await adapter.commit(inputFor(await readBoard(adapter), 'ack', { outbox: [] }, [], { effectReceipt }));
  assert.equal([...state.protected.values()].filter(row => row.value.kind === 'outbox').length, 0);
  assert.deepEqual((await readBoard(adapter)).outbox, []);
  assert.deepEqual((await adapter.receipt('ack'))!.result, { effectReceipt });
  assert.equal(state.documents.size, 1);
});

test('multi-part manifests stay bounded and obsolete manifest chunks are deleted atomically', async () => {
  const { state, adapter } = createMock();
  // Dispatch-valid operation IDs still force the roster across an 8KiB boundary.
  const id = (prefix: string, index: number): string => `${prefix}-${String(index).padStart(2, '0')}-${'x'.repeat(150)}`;
  const batches = Array.from({ length: 9 }, (_, batch) => batch);
  await batches.reduce((prior, batch) => prior.then(async () => {
    const before = await readBoard(adapter);
    const indexes = Array.from({ length: 5 }, (_, offset) => batch * 5 + offset);
    await adapter.commit(inputFor(before, `grow${batch}`, {
      tasks: [...before.tasks, ...indexes.map(index => ({ ...task(id('task', index)), status: 'running' as const, currentRun: id('run', index) }))],
      runs: [...before.runs, ...indexes.map(index => ({ id: id('run', index), taskId: id('task', index), operationId: id('run', index), status: 'reserved' as const, observedSequence: -1 }))],
    }));
  }), Promise.resolve());
  assert(Number(state.protected.get('chief-index')!.value.parts) > 1);
  assert(state.protected.has('chief-index-1'));
  assert.equal((await readBoard(adapter)).runs.length, 45);
  await batches.reduce((prior, batch) => prior.then(async () => {
    const before = await readBoard(adapter);
    const removed = new Set(before.runs.slice(0, 5).map(run => run.id));
    await adapter.commit(inputFor(before, `retire${batch}`, {
      tasks: before.tasks.map(item => removed.has(item.currentRun ?? '') ? { ...item, status: 'queued' as const, currentRun: undefined } : item),
      runs: before.runs.filter(run => !removed.has(run.id)),
    }));
  }), Promise.resolve());
  assert.equal(state.protected.get('chief-index')!.value.parts, 1);
  assert(!state.protected.has('chief-index-1'));
  assert.equal(state.protected.size, 3);
  assert.equal((await readBoard(adapter)).runs.length, 0);
});

test('receipt lookup survives turnover and retains every history/report root independently', async () => {
  const { state, adapter } = createMock();
  const report = reference('raw report');
  const first = inputFor(emptyBoard('conversation'), 'first', {}, [report]);
  await adapter.commit(first);
  const firstWireId = state.submits[0].request_id;
  await adapter.commit(inputFor(await readBoard(adapter), 'second'));
  await adapter.commit(inputFor(await readBoard(adapter), 'third'));
  assert.deepEqual(state.retained.get(firstWireId), first.artifacts.map(ref => ref.hash).sort());
  assert(state.retained.get(firstWireId)!.includes(report.hash));
  const beforeQueries = state.queries.length;
  assert.deepEqual(await adapter.receipt('first'), { ...first.metadata, revision: 1 });
  assert.deepEqual(state.queries.slice(beforeQueries).map(query => Object.keys(query)[0]), ['record_receipt']);
  assert.equal(await adapter.receipt('missing'), null);
  assert.equal(state.documents.size, 0);
  assert.equal(state.protected.size, 3, 'No per-operation state roster');
  const uncertain = inputFor(await readBoard(adapter), 'uncertain');
  state.loseReply = true;
  await assert.rejects(adapter.commit(uncertain), /lost_transport_reply/);
  assert.deepEqual(await adapter.receipt('uncertain'), { ...uncertain.metadata, revision: uncertain.value.revision });
  assert.equal(state.submits.length, 4);
});

test('real service history and receipts retire operational state without losing report roots or replay safety', async () => {
  const { state, adapter } = createMock();
  const dependencies = memory();
  const files = { ...dependencies.files, read: async (ref: FileRef): Promise<string> => {
    assert([...state.retained.values()].some(roots => roots.includes(ref.hash)), 'Only native receipt-retained artifacts may be read');
    const content = dependencies.fileWrites.get(ref.fileId);
    assert(content !== undefined);
    assert.equal(hash(content), ref.hash);
    return content;
  } };
  const service = createChiefService({ pages: adapter, files, effects: dependencies.effects }, 'conversation');
  assert((await service.execute({ kind: 'change', operationId: 'task', expectedRevision: 0, action: { kind: 'task_put', task: task('task1') } })).success);
  const dispatch = { kind: 'dispatch' as const, operationId: 'run1', expectedRevision: (await readBoard(adapter)).revision, taskId: 'task1', fresh: true };
  assert((await service.execute(dispatch)).success);
  assert.equal((await readBoard(adapter)).outbox.length, 0);
  assert.equal(dependencies.submissions.length, 1);
  const raw = 'RAW_WORKER_REPORT_DO_NOT_PASTE';
  assert((await service.receive({ kind: 'result', operationId: 'completed', runId: 'run1', jobId: 'job:run1', status: 'completed', report: raw, inboxOperationId: 'committed-input' })).success);
  const review = await readBoard(adapter);
  assert.equal(review.tasks[0].status, 'review');
  const report = review.runs[0].report;
  assert(report);
  assert((await service.execute({ kind: 'change', operationId: 'accepted', expectedRevision: review.revision,
    action: { kind: 'accept', taskId: 'task1', outcome: 'Explicitly reviewed outcome', evidence: [report] } })).success);
  const finished = await readBoard(adapter);
  assert.equal(finished.tasks[0].status, 'done');
  assert.equal(finished.runs.length, 0);
  assert.equal(finished.outbox.length, 0);
  assert.equal(state.documents.size, 1);
  assert.equal(state.protected.size, 3);
  const archived = await service.execute({ kind: 'report', runId: 'run1' });
  assert(archived.success);
  assert.deepEqual(archived.data.artifact, report);
  assert.equal(archived.data.text, raw);
  const roots = new Set([...state.retained.values()].flat());
  assert(roots.has(report.hash));
  assert([...dependencies.fileWrites.values()].every(content => roots.has(hash(content))), 'Every immutable archive/report is independently retained by a native receipt');
  const committed = state.submits.length;
  assert((await service.execute(dispatch)).success);
  assert.equal(dependencies.submissions.length, 1);
  assert.equal(state.submits.length, committed);
  assert([...state.receipts.values()].some(receipt => receipt.metadata.result?.effectReceipt !== undefined));
});

test('checkpoint and stall policy roundtrip in hidden meta without a human meta page', async () => {
  const { state, adapter } = createMock();
  const input = inputFor(emptyBoard('conversation'), 'policy', { concurrencyLimit: 2, checkinMinutes: 30,
    checkpoint: { focus: 'Review delivery', nextActions: ['Inspect acceptance evidence'] } });
  await adapter.commit(input);
  assert.deepEqual(await readBoard(adapter), input.value);
  assert.equal(state.documents.size, 0);
  assert(chunk(state, 'meta'));
});

test('provenance and footprint ride in the managed task record and its readable body', async () => {
  const { state, adapter } = createMock();
  const root = task('task1');
  const grown = { ...task('task2'), origin: 'task1', footprint: ['crates/wire/frame.rs', 'ops/deploy.sh'] };
  await adapter.commit(inputFor(emptyBoard('conversation'), 'provenance', { tasks: [root, grown] }));
  const stored = await readBoard(adapter);
  assert.deepEqual(stored.tasks.find(item => item.id === 'task2'), grown);
  // A member reading the Pages document sees what a task grew out of and what
  // its runs really touched, not only what it declared.
  const upserts = state.submits.at(-1)!.changes.flatMap(change => 'upsert' in change ? [change.upsert] : []);
  const body = upserts.find(upsert => upsert.data.value.id === 'task2')!.document.blocks[0].text;
  assert.match(body, /origin: task1/);
  assert.match(body, /crates\/wire\/frame\.rs/);
  // The largest footprint the board accepts still fits one managed record.
  const widest = { ...task('task3'), footprint: Array.from({ length: 200 }, (_, index) => `crates/services/provider/f${index}.ts`) };
  await adapter.commit(inputFor(await readBoard(adapter), 'widest', { tasks: [root, grown, widest] }));
  assert.deepEqual((await readBoard(adapter)).tasks.find(item => item.id === 'task3'), widest);
});

test('Rust Value digest includes canonical keys, protected state, metadata and artifacts', async () => {
  const { adapter } = createMock();
  const entity = { ...task('task1'), '2': 'two', '10': 'ten', '\u{1f600}': 'supplementary', '\ue000': 'private-use' };
  assert.equal((await adapter.commit(inputFor(emptyBoard('conversation'), 'canonical', { tasks: [entity] }))).kind, 'committed');
});

// -- Native budgets never split an atomic domain change ----------------------
test('document/state/change/batch/root/metadata bounds reject before any submission', async () => {
  const { state, adapter } = createMock();
  const base = emptyBoard('conversation');
  await assert.rejects(adapter.commit(inputFor(base, 'changes', { tasks: Array.from({ length: 14 }, (_, i) => task(`task${i}`)) })), /pages_atomic_change_limit/);
  await assert.rejects(adapter.commit(inputFor(base, 'data', { tasks: [{ ...task('task1'), brief: '中'.repeat(12000) }] })), /pages_record_data_too_large/);
  await assert.rejects(adapter.commit(inputFor(base, 'batch', { tasks: Array.from({ length: 8 }, (_, i) => ({ ...task(`task${i}`), brief: 'x'.repeat(8500) })) })), /pages_record_batch_too_large/);
  await assert.rejects(adapter.commit(inputFor(base, 'documents', { tasks: Array.from({ length: 1000 }, (_, i) => task(`task${i}`)),
    rules: Array.from({ length: 25 }, (_, i) => ({ id: `rule${i}`, when: 'when', instruction: 'do' })) })), /pages_record_capacity/);
  await assert.rejects(adapter.commit(inputFor(base, 'state', { tasks: Array.from({ length: 260 }, (_, i) => ({ ...task(`task${i}`), status: 'running', currentRun: `run${i}` })),
    runs: Array.from({ length: 260 }, (_, i) => ({ id: `run${i}`, taskId: `task${i}`, operationId: `run${i}`, status: 'reserved', observedSequence: -1 })) })), /pages_state_capacity/);
  await assert.rejects(adapter.commit(inputFor(base, 'roots', {}, Array.from({ length: 8 }, (_, i) => reference(`artifact${i}`)))), /pages_artifact_limit/);
  await assert.rejects(adapter.commit(inputFor(base, 'metadata', {}, [], { excessive: 'x'.repeat(8192) })), /invalid_operation_receipt/);
  const unrooted = inputFor(base, 'unrooted');
  await assert.rejects(adapter.commit({ ...unrooted, artifacts: [] }), /unretained_history_root/);
  await assert.rejects(adapter.commit({ ...unrooted, value: { ...unrooted.value, history: reference('another') } }), /history_head_mismatch/);
  assert.equal(state.submits.length, 0);
});

test('artifact roots deduplicate by hash and native retention refusal cannot partially save state', async () => {
  const { state, adapter } = createMock();
  const input = inputFor(emptyBoard('conversation'), 'deduplicate');
  await adapter.commit({ ...input, artifacts: [...input.artifacts, { ...input.metadata.history, fileId: 'another-file-name' }] });
  assert.equal(state.submits[0].artifacts.length, 1);
  const before = await readBoard(adapter);
  state.rejectArtifacts = true;
  await assert.rejects(adapter.commit(inputFor(before, 'refused', { tasks: [task('task1')] })), /native_artifact_retention_refused/);
  assert.deepEqual(await readBoard(adapter), before);
  assert.equal(await adapter.receipt('refused'), null);
});

// -- Exact fixed index, no silent empty seed or compatibility path -----------
test('missing index is empty only at collection revision zero with zero documents', async () => {
  const { state, adapter } = createMock();
  state.revision = 1;
  state.protected.set('unknown', { key: 'unknown', value: { state: 'unmanaged' }, revision: 1 });
  await assert.rejects(adapter.read(), /missing_pages_index/);
  const initialized = createMock();
  await initialized.adapter.commit(inputFor(emptyBoard('conversation'), 'init'));
  initialized.state.protected.delete('chief-index');
  await assert.rejects(initialized.adapter.read(), /missing_pages_index/);
  const nonempty = createMock();
  nonempty.state.documents.set('some-page', { record_id: 'some-page', data: { kind: 'task', value: {} }, revision: 1 });
  await assert.rejects(nonempty.adapter.read(), /missing_pages_index/);
});

test('operational documents are rejected rather than loaded through a second encoding', async () => {
  const { state, adapter } = createMock();
  await adapter.commit(inputFor(emptyBoard('conversation'), 'init'));
  state.documents.set('not-a-human-document', { record_id: 'not-a-human-document', data: { kind: 'run', value: { id: 'run1' } }, revision: 1 });
  await assert.rejects(adapter.read(), /invalid_pages_record_kind/);
});

const corruptions: { name: string; mutate: (state: MockState) => void }[] = [
  { name: 'stale index revision', mutate: state => { state.revision++; } },
  { name: 'header value/native revision mismatch', mutate: state => { state.protected.get('chief-index')!.value.revision = 0; } },
  { name: 'manifest digest', mutate: state => { state.protected.get('chief-index')!.value.digest = '0'.repeat(64); } },
  { name: 'missing manifest chunk', mutate: state => { state.protected.delete('chief-index-0'); } },
  { name: 'missing entity chunk', mutate: state => { state.protected.delete(chunk(state, 'meta').key); } },
  { name: 'missing committed history', mutate: state => {
    const row = chunk(state, 'meta');
    const value = JSON.parse(Buffer.from(String(row.value.bytes), 'base64').toString('utf8'));
    row.value.bytes = Buffer.from(JSON.stringify({ ...value, history: null })).toString('base64');
  } },
  { name: 'wrong total', mutate: state => { chunk(state, 'meta').value.total = 2; } },
  { name: 'wrong part', mutate: state => { chunk(state, 'meta').value.part = 1; } },
  { name: 'noncanonical base64', mutate: state => { chunk(state, 'meta').value.bytes = '!!!!'; } },
  { name: 'invalid UTF8', mutate: state => { chunk(state, 'meta').value.bytes = Buffer.from([255]).toString('base64'); } },
];
corruptions.forEach(({ name, mutate }) => test(`protected state fails closed for ${name}`, async () => {
  const { state, adapter } = createMock();
  await adapter.commit(inputFor(emptyBoard('conversation'), 'init'));
  mutate(state);
  await assert.rejects(adapter.read());
}));

test('32-document paging and final revision checks reject movement without retrying', async () => {
  const { state, adapter } = createMock();
  await [0, 1, 2, 3].reduce((prior, batch) => prior.then(async () => {
    const before = await readBoard(adapter);
    await adapter.commit(inputFor(before, `batch${batch}`, { tasks: [...before.tasks, ...Array.from({ length: 10 }, (_, i) => task(`task${batch}-${i}`))] }));
  }), Promise.resolve());
  assert.equal((await readBoard(adapter)).tasks.length, 40);
  const from = state.queries.length;
  state.onQuery = input => {
    const records = input.records as { after: string | null } | undefined;
    if (records?.after) state.revision++;
  };
  await assert.rejects(adapter.read(), /revision_conflict/);
  assert.equal(state.queries.slice(from).filter(query => query.records).length, 2);
  const fresh = createMock();
  await fresh.adapter.commit(inputFor(emptyBoard('conversation'), 'initial'));
  const previousHeaders = fresh.state.queries.filter(input => input.record_collection).length;
  fresh.state.onQuery = input => {
    const finalHeader = input.record_collection && fresh.state.queries.filter(query => query.record_collection).length === previousHeaders + 2;
    if (finalHeader) fresh.state.revision++;
  };
  await assert.rejects(fresh.adapter.read(), /revision_conflict/);
});

const receiptCorruptions: { name: string; mutate: (receipt: Receipt) => Receipt }[] = [
  { name: 'digest', mutate: receipt => ({ ...receipt, payload_digest: Array<number>(32).fill(0) }) },
  { name: 'missing history retention', mutate: receipt => ({ ...receipt, artifacts: [] }) },
  { name: 'different artifact roots', mutate: receipt => ({ ...receipt, artifacts: [...receipt.artifacts, 'a'.repeat(64)] }) },
  { name: 'revision', mutate: receipt => ({ ...receipt, revision: 900 }) },
  { name: 'page', mutate: receipt => ({ ...receipt, page_id: 'other' }) },
  { name: 'request', mutate: receipt => ({ ...receipt, request_id: 'other' }) },
  { name: 'metadata fingerprint', mutate: receipt => ({ ...receipt, metadata: { ...receipt.metadata, fingerprint: 'b'.repeat(64) } }) },
  { name: 'metadata history', mutate: receipt => ({ ...receipt, metadata: { ...receipt.metadata, history: reference('wrong') } }) },
];
receiptCorruptions.forEach(({ name, mutate }) => test(`admission is not an authoritative matching receipt: ${name}`, async () => {
  const { state, adapter } = createMock();
  state.tamperReceipt = mutate;
  await assert.rejects(adapter.commit(inputFor(emptyBoard('conversation'), 'tampered')), /invalid_.*receipt/);
}));

test('pre-aborted reads and receipt lookups never query or mutate', async () => {
  const { state, adapter } = createMock();
  await assert.rejects(adapter.read(AbortSignal.abort()));
  await assert.rejects(adapter.receipt('operation', AbortSignal.abort()));
  assert.equal(state.queries.length, 0);
  assert.equal(state.submits.length, 0);
});
