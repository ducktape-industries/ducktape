// Pages receipts own idempotency; the board is only current policy state. Every
// mutation rereads authority, validates a pure step, writes a pinned Files delta,
// then CASes state + receipt metadata. Uncommitted Files writes never imply save.
import { createHash } from 'node:crypto';

import type { ArtifactAnchor, Board, FileRef, FilesAdapter, OperationReceipt, PagesAdapter, Run, Task } from './contracts.ts';
import { PolicyError, requirePolicy, retainCurrentRuns, validId, validRefs, validateBoard } from './domain.ts';

// -- Stable operation identity and bounded immutable history -----------------
const compareKeys = (a: string, b: string): number => { if (a === b) return 0; return a < b ? -1 : 1; };
const canonical = (value: unknown): unknown => {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value)
    .sort(([a], [b]) => compareKeys(a, b)).map(([key, item]) => [key, canonical(item)]));
  return value;
};
export const fingerprint = (value: unknown): string => createHash('sha256').update(JSON.stringify(canonical(value))).digest('hex');
const entityKey = (value: unknown): string => {
  const entity = value as { id?: string; operationId?: string };
  return entity.id ?? entity.operationId!;
};
const historyDelta = (before: Board, after: Board): Record<string, unknown> => {
  const kinds = ['tasks', 'runs', 'asks', 'rules', 'outbox'] as const;
  const collections = Object.fromEntries(kinds.map(kind => {
    const previous = new Map((before[kind] as unknown[]).map(value => [entityKey(value), fingerprint(value)]));
    const next = new Map((after[kind] as unknown[]).map(value => [entityKey(value), value]));
    return [kind, { upsert: [...next].filter(([id, value]) => previous.get(id) !== fingerprint(value)).map(([, value]) => value),
      delete: [...previous.keys()].filter(id => !next.has(id)) }];
  }));
  return { ...collections, checkpoint: after.checkpoint, concurrencyLimit: after.concurrencyLimit, checkinMinutes: after.checkinMinutes };
};
const artifactRefs = (value: unknown): FileRef[] => {
  if (Array.isArray(value)) return value.flatMap(artifactRefs);
  if (!value || typeof value !== 'object') return [];
  const record = value as Record<string, unknown>;
  if (validRefs([record as unknown as FileRef])) return [record as unknown as FileRef];
  return Object.values(record).flatMap(artifactRefs);
};
interface HistoryRecord {
  operationId: string; fingerprint: string; revision: number; previous: FileRef | null;
  delta: { runs: { upsert: Run[] }; tasks: { upsert: Task[] } };
}
export interface ReportArtifact { taskId: string; artifact: FileRef; anchor: ArtifactAnchor }
export const MAX_HISTORY_LOOKUP_RECORDS = 10000;
const identity = (ref: FileRef): FileRef => ({ fileId: ref.fileId, hash: ref.hash });
const sameRef = (left: FileRef, right: FileRef): boolean => left.fileId === right.fileId && left.hash === right.hash;
const runArtifacts = (run: Run | undefined): FileRef[] => run
  ? [run.report, run.progress?.source, ...(run.progress?.artifacts ?? [])].filter((ref): ref is FileRef => ref !== undefined) : [];
export interface ChangeResult { board: Board; revision: number; operationId: string; replayed: boolean }
export interface ChiefStore {
  read(signal?: AbortSignal): Promise<Board>;
  receipt(operationId: string, signal?: AbortSignal): Promise<OperationReceipt | null>;
  reportArtifact(input: { runId: string; artifact?: FileRef; anchor?: ArtifactAnchor }, signal?: AbortSignal): Promise<ReportArtifact>;
  change(input: { operationId: string; expectedRevision: number; intent: unknown; result?: Record<string, unknown>; transform: (board: Board) => Board }, signal?: AbortSignal): Promise<ChangeResult>;
}
export const createStore = (pages: PagesAdapter, conversationId: string, files: FilesAdapter): ChiefStore => {
  const read = (signal?: AbortSignal): Promise<Board> => Promise.resolve()
    .then(() => pages.read(signal))
    .then(snapshot => {
      const board = validateBoard(snapshot.value, conversationId);
      requirePolicy(snapshot.revision === board.revision, 'record_revision_mismatch');
      return board;
    });
  const receipt = (operationId: string, signal?: AbortSignal): Promise<OperationReceipt | null> => Promise.resolve()
    .then(() => pages.receipt(operationId, signal))
    .then(value => {
      if (!value) return null;
      requirePolicy(value.operationId === operationId && /^[a-f0-9]{64}$/.test(value.fingerprint)
        && Number.isSafeInteger(value.revision) && value.revision > 0 && validRefs([value.history]), 'invalid_operation_receipt');
      return value;
    });
  const historyRecord = (ref: FileRef, signal?: AbortSignal): Promise<HistoryRecord> => Promise.resolve()
    .then(() => files.read(ref, signal))
    .then(raw => {
      const record = JSON.parse(raw) as HistoryRecord;
      requirePolicy(validId(record.operationId) && /^[a-f0-9]{64}$/.test(record.fingerprint) && Number.isSafeInteger(record.revision)
        && Array.isArray(record.delta?.runs?.upsert) && Array.isArray(record.delta?.tasks?.upsert)
        && (record.previous === null || validRefs([record.previous])), 'invalid_history_record');
      return record;
    });
  const verifyHistory = (record: HistoryRecord, saved: OperationReceipt, ref: FileRef): HistoryRecord => {
    requirePolicy(sameRef(saved.history, ref) && saved.operationId === record.operationId && saved.fingerprint === record.fingerprint
      && saved.revision === record.revision, 'wrong_history_anchor');
    return record;
  };
  const anchoredHistory = (anchor: ArtifactAnchor, signal?: AbortSignal): Promise<HistoryRecord> => Promise.resolve()
    .then(() => receipt(anchor.operationId, signal))
    .then(saved => {
      requirePolicy(saved && validRefs([anchor.history]) && sameRef(saved.history, anchor.history), 'wrong_history_anchor');
      return historyRecord(saved!.history, signal).then(record => verifyHistory(record, saved!, anchor.history));
    });
  const reportArtifact: ChiefStore['reportArtifact'] = (input, signal) => Promise.resolve()
    .then(() => {
      requirePolicy(!input.anchor || input.artifact, 'anchor_requires_artifact');
      return receipt(input.runId, signal);
    })
    .then(origin => {
      requirePolicy(origin, 'report_unavailable');
      return historyRecord(origin!.history, signal).then(record => verifyHistory(record, origin!, origin!.history));
    })
    .then(origin => {
      // Dispatch's immutable receipt proves which task owns this canonical run,
      // independently of current state and any caller-supplied cursor.
      const run = origin.delta.runs.upsert.find(run => run.id === input.runId && run.operationId === input.runId);
      requirePolicy(run && validId(run.taskId), 'report_unavailable');
      const taskId = run!.taskId;
      const select = (record: HistoryRecord): FileRef | undefined => {
        const run = record.delta.runs.upsert.find(run => run.id === input.runId && run.operationId === input.runId && run.taskId === taskId);
        if (!input.artifact) return run?.report ?? run?.progress?.source;
        const task = record.delta.tasks.upsert.find(task => task.id === taskId);
        return [...runArtifacts(run), ...(task?.evidence ?? []), ...(task?.acceptance?.evidence ?? [])].find(ref => sameRef(ref, input.artifact!));
      };
      const resolved = (record: HistoryRecord, ref: FileRef, artifact: FileRef): ReportArtifact => ({ taskId, artifact: identity(artifact),
        anchor: { operationId: record.operationId, history: identity(ref) } });
      if (input.anchor) return anchoredHistory(input.anchor, signal).then(record => {
        const artifact = select(record);
        requirePolicy(artifact, 'unknown_evidence_reference');
        return resolved(record, input.anchor!.history, artifact!);
      });
      const seen = new Set<string>();
      const visit = (ref: FileRef | null): Promise<ReportArtifact> => Promise.resolve()
        .then(() => {
          requirePolicy(ref, input.artifact ? 'unknown_evidence_reference' : 'report_unavailable');
          const key = fingerprint(ref);
          requirePolicy(!seen.has(key) && seen.size < MAX_HISTORY_LOOKUP_RECORDS, 'history_lookup_bound');
          seen.add(key);
          return historyRecord(ref!, signal);
        })
        .then(record => {
          const artifact = select(record);
          if (!artifact) return visit(record.previous);
          return anchoredHistory({ operationId: record.operationId, history: ref! }, signal)
            .then(() => resolved(record, ref!, artifact));
        });
      return read(signal).then(board => visit(board.history));
    });
  return {
    read, receipt, reportArtifact,
    change: (input, signal) => Promise.resolve()
      .then(() => {
        requirePolicy(validId(input.operationId) && input.operationId.length <= 160, 'invalid_operation_id');
        requirePolicy(Number.isSafeInteger(input.expectedRevision) && input.expectedRevision >= 0, 'invalid_revision');
        return receipt(input.operationId, signal);
      })
      .then(prior => read(signal).then(board => ({ prior, board })))
      .then(({ prior, board }) => {
        const digest = fingerprint(input.intent);
        if (prior) {
          requirePolicy(prior.fingerprint === digest, 'operation_id_reused');
          return { board, operationId: input.operationId, revision: prior.revision, replayed: true };
        }
        requirePolicy(board.revision === input.expectedRevision, 'revision_conflict');
        const changed = retainCurrentRuns(input.transform(structuredClone(board)));
        requirePolicy(changed.revision === board.revision, 'transition_changed_revision');
        const revision = board.revision + 1;
        const next = validateBoard({ ...changed, revision }, conversationId);
        const history = { operationId: input.operationId, fingerprint: digest, revision, previous: board.history,
          delta: historyDelta(board, next), ...(input.result ? { result: input.result } : {}) };
        return Promise.resolve()
          .then(() => files.put({ operationId: `history-${fingerprint(history)}`, content: JSON.stringify(canonical(history)) }, signal))
          .then(ref => {
            requirePolicy(validRefs([ref]), 'invalid_history_ref');
            const value = { ...next, history: ref };
            const metadata = { operationId: input.operationId, fingerprint: digest, history: ref, ...(input.result ? { result: input.result } : {}) };
            // Immutable earlier receipts already retain existing roots. Repeating
            // the complete archive ancestry would exhaust the native8-root cap.
            const retained = new Set(artifactRefs(board).map(artifact => artifact.hash));
            const introduced = artifactRefs(history).filter(artifact => !retained.has(artifact.hash));
            const artifacts = [...new Map([...introduced, ref].map(artifact => [artifact.hash, artifact])).values()];
            requirePolicy(artifacts.length <= 8, 'artifact_batch_capacity');
            return pages.commit({ expectedRevision: board.revision, requestId: input.operationId, value, metadata, artifacts }, signal)
              .then(committed => ({ committed, value }));
          })
          .then(({ committed, value }) => {
            if (committed.kind === 'conflict') throw new PolicyError('revision_conflict');
            requirePolicy(committed.requestId === input.operationId && committed.revision === revision, 'invalid_commit_receipt');
            return { board: value, operationId: input.operationId, revision, replayed: false };
          });
      }),
  };
};
