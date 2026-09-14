// Explicit evidence reads stay scoped to retained work, preserve every cursor,
// and fit the real tool formatter even after JSON escaping and Unicode encoding.
import assert from 'node:assert/strict';
import test from 'node:test';
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';

import type { ArtifactAnchor, FileRef } from './contracts.ts';
import { createChiefService } from './service.ts';
import { fingerprint, MAX_HISTORY_LOOKUP_RECORDS } from './store.ts';
import { CHIEF_TOOL_NAMES, chiefToolSchemas, registerChiefTools } from './tools.ts';
import { conversationId, memory, task } from './test-support.ts';

const fixture = async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  assert((await service.execute({ kind: 'change', operationId: 'task', expectedRevision: 0, action: { kind: 'task_put', task: task() } })).success);
  assert((await service.execute({ kind: 'dispatch', operationId: 'run', expectedRevision: 1, taskId: 'task-1', fresh: true })).success);
  return { adapters, service };
};
interface Window { artifact: FileRef; anchor: ArtifactAnchor; snapshot: string; offset: number; nextOffset: number | null; eof: boolean; total: number; text: string; partialRead: boolean; provenance: string; offsetUnit: string }
interface ReaderTool { execute(id: string, args: Record<string, unknown>, signal: AbortSignal): Promise<{ content: { text: string }[]; details: { truncated: boolean } }> }

test('Chief inventory exposes a readable multi-window report without losing escaped/Unicode cursors', async () => {
  const { adapters, service } = await fixture();
  const source = '\0"\\\n🙂\u2028'.repeat(5000);
  assert((await service.receive({ kind: 'result', operationId: 'result', runId: 'run', jobId: 'job:run', status: 'completed', report: source, inboxOperationId: 'native-result' })).success);
  const registered = new Map<string, unknown>();
  registerChiefTools({ registerTool: (tool: unknown) => registered.set((tool as { name: string }).name, tool) } as unknown as ExtensionAPI, service);
  assert.deepEqual([...registered.keys()].sort(), [...CHIEF_TOOL_NAMES].sort());
  assert.equal((chiefToolSchemas.chief_report.properties.limit as unknown as { maximum: number }).maximum, 2000);
  const reader = registered.get('chief_report') as ReaderTool;
  const before = fingerprint(adapters.state.board);
  const fileCount = adapters.fileWrites.size;
  const parts: string[] = [];
  const cursor: { offset: number; artifact?: FileRef; anchor?: ArtifactAnchor } = { offset: 0 };
  for (;;) {
    const result = await reader.execute('read', { runId: 'run', ...cursor, limit: 2000 }, new AbortController().signal);
    assert.equal(result.details.truncated, false);
    const formatted = result.content[0].text;
    assert(Buffer.byteLength(formatted, 'utf8') <= 24000);
    assert(formatted.split('\n').length <= 500);
    const envelope = JSON.parse(formatted) as { success: boolean; data: Window };
    assert(envelope.success);
    const page = envelope.data;
    assert.equal(page.offsetUnit, 'unicode_codepoints');
    assert.equal(page.offset, cursor.offset);
    assert.equal(page.total, Array.from(source).length);
    assert.equal(page.snapshot, page.artifact.hash);
    assert.equal(page.partialRead, true);
    assert.match(page.provenance, /untrusted/);
    if (cursor.artifact) assert.deepEqual(page.artifact, cursor.artifact);
    parts.push(page.text);
    if (page.eof) { assert.equal(page.nextOffset, null); break; }
    assert(page.nextOffset! > cursor.offset);
    cursor.offset = page.nextOffset!;
    cursor.artifact = page.artifact;
    cursor.anchor = page.anchor;
  }
  assert.equal(parts.join(''), source);
  assert.equal(fingerprint(adapters.state.board), before);
  assert.equal(adapters.fileWrites.size, fileCount);
});

test('checkpoint sources and validated retained evidence are readable; unrelated refs are refused', async () => {
  const { adapters, service } = await fixture();
  const evidence = await adapters.files.put({ operationId: 'evidence', content: 'Concrete verification output' });
  adapters.retained.add(evidence.hash); // Existing worker-owned source, before Chief promotes it.
  const progress = { sequence: 1, summary: 'Verified the test output', next: 'Review', artifacts: [evidence] };
  const raw = JSON.stringify(progress);
  assert((await service.receive({ kind: 'progress', operationId: 'progress', runId: 'run', jobId: 'job:run', progress, raw, inboxOperationId: 'native-progress' })).success);
  const checkpoint = await service.execute({ kind: 'report', runId: 'run' });
  assert(checkpoint.success);
  assert.equal(checkpoint.data.text, raw);
  const read = await service.execute({ kind: 'report', runId: 'run', artifact: evidence });
  assert(read.success);
  assert.equal(read.data.text, 'Concrete verification output');
  assert.equal(read.data.partialRead, false);
  const foreign = await adapters.files.put({ operationId: 'unrelated', content: 'Not this task' });
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run', artifact: foreign }), { success: false, error: 'unknown_evidence_reference' });
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run', limit: 2001 }), { success: false, error: 'invalid_report_window' });
});

test('receipt-anchored checkpoint windows survive replacement and refuse foreign membership', async () => {
  const { adapters, service } = await fixture();
  const progress = { sequence: 1, summary: 'First evidence', next: 'Continue', artifacts: [] };
  const raw = JSON.stringify({ ...progress, detail: '🦆'.repeat(800) });
  assert((await service.receive({ kind: 'progress', operationId: 'checkpoint-first', runId: 'run', jobId: 'job:run', progress, raw, inboxOperationId: 'native-first' })).success);
  const initial = await service.execute({ kind: 'report', runId: 'run', limit: 32 });
  assert(initial.success);
  const first = initial.data as unknown as Window;
  const second = { ...progress, sequence: 2, summary: 'Replacement checkpoint' };
  assert((await service.receive({ kind: 'progress', operationId: 'checkpoint-second', runId: 'run', jobId: 'job:run', progress: second,
    raw: JSON.stringify(second), inboxOperationId: 'native-second' })).success);
  assert.notDeepEqual(adapters.state.board.runs[0].progress?.source, first.artifact);
  const originalRead = adapters.files.read;
  const reads: FileRef[] = [];
  adapters.files.read = (ref, signal) => { reads.push(ref); return originalRead(ref, signal); };
  const before = fingerprint(adapters.state.board);
  const rest = await service.execute({ kind: 'report', runId: 'run', artifact: first.artifact, anchor: first.anchor, offset: first.nextOffset!, limit: 2000 });
  assert(rest.success, JSON.stringify(rest));
  assert.equal(first.text + rest.data.text, raw);
  assert.deepEqual(rest.data.anchor, first.anchor);
  assert.equal(rest.data.nextOffset, null);
  assert.equal(reads.length, 3, 'Only dispatch provenance, anchored membership and selected artifact are read');
  assert.equal(fingerprint(adapters.state.board), before);
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run', artifact: first.artifact, offset: 32 }),
    { success: false, error: 'report_cursor_requires_anchor' });
  const foreign = await adapters.files.put({ operationId: 'foreign', content: 'Unrelated private content' });
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run', artifact: foreign, anchor: first.anchor }),
    { success: false, error: 'unknown_evidence_reference' });
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run', artifact: first.artifact, anchor: { ...first.anchor, history: foreign } }),
    { success: false, error: 'wrong_history_anchor' });
  assert(!reads.some(ref => ref.fileId === foreign.fileId));
  assert((await service.execute({ kind: 'change', operationId: 'task-other', expectedRevision: adapters.state.board.revision,
    action: { kind: 'task_put', task: task('other') } })).success);
  assert((await service.execute({ kind: 'dispatch', operationId: 'run-other', expectedRevision: adapters.state.board.revision, taskId: 'other', fresh: true })).success);
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run-other', artifact: first.artifact, anchor: first.anchor }),
    { success: false, error: 'unknown_evidence_reference' });
});

test('initial history lookup fails honestly at its bound; an existing cursor never scans newer history', async () => {
  const { adapters, service } = await fixture();
  assert((await service.receive({ kind: 'result', operationId: 'result', runId: 'run', jobId: 'job:run', status: 'completed', report: 'Durable evidence', inboxOperationId: 'native-result' })).success);
  const result = await service.execute({ kind: 'report', runId: 'run', limit: 1 });
  assert(result.success);
  const first = result.data as unknown as Window;
  const originalRead = adapters.files.read;
  const counts = { newer: 0 };
  const reference = (index: number): FileRef => ({ fileId: `newer-${index}`, hash: 'b'.repeat(64) });
  adapters.state.board.history = reference(0);
  adapters.files.read = (ref, signal) => {
    if (!ref.fileId.startsWith('newer-')) return originalRead(ref, signal);
    const index = Number(ref.fileId.slice('newer-'.length));
    counts.newer++;
    return Promise.resolve(JSON.stringify({ operationId: ref.fileId, fingerprint: 'a'.repeat(64), revision: MAX_HISTORY_LOOKUP_RECORDS + 1000 - index,
      previous: reference(index + 1), delta: { runs: { upsert: [] }, tasks: { upsert: [] } } }));
  };
  assert.deepEqual(await service.execute({ kind: 'report', runId: 'run' }), { success: false, error: 'history_lookup_bound' });
  assert.equal(counts.newer, MAX_HISTORY_LOOKUP_RECORDS);
  const rest = await service.execute({ kind: 'report', runId: 'run', artifact: first.artifact, anchor: first.anchor, offset: 1 });
  assert(rest.success);
  assert.equal(first.text + rest.data.text, 'Durable evidence');
  assert.equal(counts.newer, MAX_HISTORY_LOOKUP_RECORDS);
});
