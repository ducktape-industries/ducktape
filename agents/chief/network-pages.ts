// Human work is ordinary Pages documents; runtime entities are protected,
// chunked state. One collection CAS binds documents, the current-state index,
// and receipt-owned Files roots. Historical operations never become documents.
import { createHash } from 'node:crypto';

import type { Board, CommitReceipt, FileRef, OperationMetadata, OperationReceipt, PagesAdapter, RecordSnapshot } from './contracts.ts';
import { assertNever, emptyBoard, requirePolicy, validId, validRefs, validateBoard } from './domain.ts';

// -- Generic authenticated module boundary ----------------------------------
export interface NetworkPagesRpc {
  query(module: string, input: Record<string, unknown>, signal?: AbortSignal): Promise<unknown>;
  // Runs encodes the message as serde_json::Value. Receipts hash that canonical
  // module payload, not JavaScript property insertion order.
  submit(module: string, input: Record<string, unknown>, requestId: string, signal?: AbortSignal): Promise<unknown>;
}
type DocumentKind = 'task' | 'ask' | 'rule';
type HiddenKind = 'meta' | 'run' | 'outbox';
type EntityKind = DocumentKind | HiddenKind;
interface Entity<Kind extends EntityKind = EntityKind> { kind: Kind; value: unknown }
interface Collection { revision: number; count: number }
interface ManagedRecord { recordId: string; data: Entity<DocumentKind>; revision: number }
interface StateWrite { key: string; value: unknown }
interface StoredState extends StateWrite { revision: number }
interface IndexHeader { revision: number; parts: number; digest: string }
interface IndexEntry { kind: HiddenKind; entityId: string; parts: number }
interface Document { title: string; blocks: { id: string; kind: 'paragraph'; text: string; marks: never[] }[] }
type DocumentChange = { upsert: { record_id: string; data: Entity<DocumentKind>; document: Document } } | { delete: { record_id: string } };
type StateChange = { put: StateWrite } | { delete: { key: string } };
interface Snapshot { collection: Collection; records: ManagedRecord[]; states: StoredState[]; board: Board }
interface CheckedReceipt { receipt: OperationReceipt; digest: number[]; artifacts: string[] }
const INDEX_KEY = 'chief-index';
const RECORD_LIMIT = 1024;
const STATE_LIMIT = 256;
const PAGE_LIMIT = 32;
const CHANGE_LIMIT = 16;
const DATA_BYTES = 32 * 1024;
const STATE_BYTES = 16 * 1024;
const CHUNK_BYTES = 8 * 1024;
const METADATA_BYTES = 8 * 1024;
const BATCH_BYTES = 128 * 1024;
const ARTIFACT_LIMIT = 8;
const documentKinds = new Set<string>(['task', 'ask', 'rule']);
const hiddenKinds = new Set<string>(['meta', 'run', 'outbox']);

// -- Exact serialization and identities -------------------------------------
const object = (value: unknown): Record<string, unknown> => {
  const valid = typeof value === 'object' && value !== null && !Array.isArray(value);
  requirePolicy(valid, 'invalid_pages_record_response');
  return value as Record<string, unknown>;
};
const integer = (value: unknown): number => {
  requirePolicy(Number.isSafeInteger(value) && Number(value) >= 0, 'invalid_pages_record_revision');
  return value as number;
};
const string = (value: unknown): string => {
  requirePolicy(typeof value === 'string' && value.length > 0, 'invalid_pages_record_identity');
  return value as string;
};
const digest = (value: string | Uint8Array): string => createHash('sha256').update(value).digest('hex');
const bytes = (value: unknown): number => Buffer.byteLength(JSON.stringify(value), 'utf8');
const compareText = (left: string, right: string): number => Buffer.compare(Buffer.from(left), Buffer.from(right));
// Manual objects preserve Rust's UTF-8 key order, including integer-looking
// keys. Product numbers are safe integers, avoiding ambiguous float formatting.
const rustValueJson = (value: unknown): string => {
  if (value === null) return 'null';
  if (Array.isArray(value)) return `[${value.map(item => rustValueJson(item ?? null)).join(',')}]`;
  if (typeof value === 'object') return `{${Object.entries(object(value)).filter(([, item]) => item !== undefined)
    .toSorted(([left], [right]) => compareText(left, right))
    .map(([key, item]) => `${JSON.stringify(key)}:${rustValueJson(item)}`).join(',')}}`;
  const isNumber = typeof value === 'number';
  requirePolicy(!isNumber || Number.isSafeInteger(value), 'unsupported_pages_record_number');
  requirePolicy(isNumber || typeof value === 'string' || typeof value === 'boolean', 'invalid_pages_record_data');
  return JSON.stringify(value);
};
const same = (left: unknown, right: unknown): boolean => rustValueJson(left) === rustValueJson(right);
const entityId = (data: Entity): string => {
  switch (data.kind) {
    case 'meta': return 'meta';
    case 'task': return string(object(data.value).id);
    case 'run': return string(object(data.value).id);
    case 'ask': return string(object(data.value).id);
    case 'rule': return string(object(data.value).id);
    case 'outbox': return string(object(data.value).operationId);
    default: return assertNever(data.kind);
  }
};
const storageId = (pageId: string, kind: EntityKind, id: string): string => `chief-${digest(JSON.stringify([pageId, kind, id]))}`;
const recordId = (pageId: string, data: Entity): string => storageId(pageId, data.kind, entityId(data));
export const managedRecordPageId = (pageId: string, kind: DocumentKind, entityId: string): string => storageId(pageId, kind, entityId);
export const managedRecordTargetMatches = (pageId: string, kind: DocumentKind, entityId: string, targetId: string): boolean => {
  const id = managedRecordPageId(pageId, kind, entityId);
  return targetId === id || targetId === `${id}-body`;
};
const hiddenKey = (pageId: string, kind: HiddenKind, id: string, part: number): string => `${storageId(pageId, kind, id)}-part-${part}`;
const indexKey = (part: number): string => `${INDEX_KEY}-${part}`;
const requestId = (pageId: string, operationId: string): string => `chief-request-${digest(JSON.stringify([pageId, operationId]))}`;

// -- Receipt identity, metadata and native artifact roots --------------------
const parseMetadata = (raw: unknown, operationId: string): OperationMetadata => {
  const metadata = object(raw);
  const fields = new Set(['operationId', 'fingerprint', 'history', 'result']);
  const hasIdentity = metadata.operationId === operationId && validId(operationId) && operationId.length <= 160;
  const hasFingerprint = typeof metadata.fingerprint === 'string' && /^[a-f0-9]{64}$/.test(metadata.fingerprint);
  requirePolicy(hasIdentity && hasFingerprint && validRefs([metadata.history as FileRef])
    && Object.keys(metadata).every(key => fields.has(key)) && bytes(metadata) <= METADATA_BYTES, 'invalid_operation_receipt');
  if (metadata.result !== undefined) object(metadata.result);
  return metadata as unknown as OperationMetadata;
};
const artifactHashes = (artifacts: FileRef[], metadata: OperationMetadata): string[] => {
  requirePolicy(Array.isArray(artifacts) && artifacts.every(ref => validRefs([ref])), 'invalid_pages_artifacts');
  const roots = [...new Set(artifacts.map(ref => ref.hash))].sort();
  requirePolicy(roots.length <= ARTIFACT_LIMIT, 'pages_artifact_limit');
  requirePolicy(roots.includes(metadata.history.hash), 'unretained_history_root');
  return roots;
};
const parseReceipt = (raw: unknown, pageId: string, operationId: string): CheckedReceipt | null => {
  const value = object(raw).record_receipt;
  if (value === null) return null;
  const receipt = object(value);
  const hasIdentity = receipt.page_id === pageId && receipt.request_id === requestId(pageId, operationId);
  const hash = receipt.payload_digest;
  const validDigest = Array.isArray(hash) && hash.length === 32 && hash.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255);
  const revision = integer(receipt.revision);
  requirePolicy(hasIdentity && validDigest && revision > 0, 'invalid_operation_receipt');
  const metadata = parseMetadata(receipt.metadata, operationId);
  const artifacts = receipt.artifacts;
  const retained = Array.isArray(artifacts) && artifacts.length <= ARTIFACT_LIMIT
    && artifacts.every(root => typeof root === 'string' && /^[a-f0-9]{64}$/.test(root))
    && new Set(artifacts).size === artifacts.length && artifacts.includes(metadata.history.hash);
  requirePolicy(retained, 'invalid_operation_receipt');
  return { receipt: { ...metadata, revision }, digest: hash as number[], artifacts: artifacts as string[] };
};

// -- One chunk codec for the index and all protected entities ----------------
const encodeChunks = (kind: HiddenKind | 'index', id: string, content: Buffer, key: (part: number) => string): StateWrite[] => {
  const total = Math.ceil(content.length / CHUNK_BYTES);
  requirePolicy(total > 0 && total < STATE_LIMIT, 'pages_state_capacity');
  return Array.from({ length: total }, (_, part) => ({ key: key(part), value: { kind, entityId: id, part, total,
    bytes: content.subarray(part * CHUNK_BYTES, (part + 1) * CHUNK_BYTES).toString('base64') } }));
};
const decodeChunks = (rows: StoredState[], kind: HiddenKind | 'index', id: string, total: number): Buffer => {
  requirePolicy(rows.length === total, 'incomplete_pages_state_entity');
  return Buffer.concat(rows.map((row, part) => {
    const value = object(row.value);
    const exact = Object.keys(value).length === 5 && value.kind === kind && value.entityId === id && value.part === part && value.total === total;
    requirePolicy(exact && bytes(value) <= STATE_BYTES, 'invalid_pages_state_chunk');
    const encoded = string(value.bytes);
    const content = Buffer.from(encoded, 'base64');
    const canonical = content.length > 0 && content.length <= CHUNK_BYTES && content.toString('base64') === encoded;
    const fullPart = part === total - 1 || content.length === CHUNK_BYTES;
    requirePolicy(canonical && fullPart, 'invalid_pages_state_bytes');
    return content;
  }));
};
const decodeJson = (content: Buffer): unknown => JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(content));
const compareEntries = (left: IndexEntry, right: IndexEntry): number => compareText(left.kind, right.kind) || compareText(left.entityId, right.entityId);
const parseHeader = (row: StoredState, revision: number): IndexHeader => {
  const header = object(row.value);
  requirePolicy(Object.keys(header).length === 3 && integer(header.revision) === row.revision, 'invalid_pages_index');
  requirePolicy(row.revision === revision, 'revision_conflict');
  const parts = integer(header.parts);
  requirePolicy(parts > 0 && parts < STATE_LIMIT && typeof header.digest === 'string' && /^[a-f0-9]{64}$/.test(header.digest), 'invalid_pages_index');
  return { revision, parts, digest: header.digest as string };
};
const parseManifest = (content: Buffer, header: IndexHeader): IndexEntry[] => {
  requirePolicy(digest(content) === header.digest, 'invalid_pages_index_digest');
  const value = decodeJson(content);
  requirePolicy(Array.isArray(value) && value.length > 0 && value.length < STATE_LIMIT, 'invalid_pages_index');
  const entries = (value as unknown[]).map(item => {
    const entry = object(item);
    const kind = string(entry.kind);
    const id = string(entry.entityId);
    const parts = integer(entry.parts);
    requirePolicy(Object.keys(entry).length === 3 && hiddenKinds.has(kind) && validId(id) && parts > 0 && parts < STATE_LIMIT, 'invalid_pages_index');
    requirePolicy(kind !== 'meta' || id === 'meta', 'invalid_pages_index');
    return { kind: kind as HiddenKind, entityId: id, parts };
  });
  const ordered = entries.every((entry, index) => index === 0 || compareEntries(entries[index - 1], entry) < 0);
  const count = 1 + header.parts + entries.reduce((sum, entry) => sum + entry.parts, 0);
  requirePolicy(ordered && count <= STATE_LIMIT, 'pages_state_capacity');
  return entries;
};

// -- Current product state; no operation-log roster --------------------------
const documentsFor = (board: Board): Entity<DocumentKind>[] => [
  ...board.tasks.map(value => ({ kind: 'task' as const, value })),
  ...board.asks.map(value => ({ kind: 'ask' as const, value })),
  ...board.rules.map(value => ({ kind: 'rule' as const, value })),
];
const hiddenFor = (board: Board): Entity<HiddenKind>[] => [
  { kind: 'meta', value: { conversationId: board.conversationId, concurrencyLimit: board.concurrencyLimit,
    checkpoint: board.checkpoint, checkinMinutes: board.checkinMinutes, history: board.history } },
  ...board.runs.map(value => ({ kind: 'run' as const, value })),
  ...board.outbox.map(value => ({ kind: 'outbox' as const, value })),
];
const statesFor = (pageId: string, board: Board): StateWrite[] => {
  const encoded = hiddenFor(board).map(entity => {
    const id = entityId(entity);
    const rows = encodeChunks(entity.kind, id, Buffer.from(rustValueJson(entity.value), 'utf8'), part => hiddenKey(pageId, entity.kind, id, part));
    return { entry: { kind: entity.kind, entityId: id, parts: rows.length }, rows };
  }).toSorted((left, right) => compareEntries(left.entry, right.entry));
  const manifest = Buffer.from(rustValueJson(encoded.map(item => item.entry)), 'utf8');
  const index = encodeChunks('index', INDEX_KEY, manifest, indexKey);
  const states: StateWrite[] = [{ key: INDEX_KEY, value: { revision: board.revision, parts: index.length, digest: digest(manifest) } },
    ...index, ...encoded.flatMap(item => item.rows)];
  requirePolicy(states.length <= STATE_LIMIT && new Set(states.map(row => row.key)).size === states.length, 'pages_state_capacity');
  states.forEach(row => requirePolicy(bytes(row.value) <= STATE_BYTES, 'pages_state_value_too_large'));
  return states;
};
const values = (entities: Entity[], kind: EntityKind): unknown[] => entities.filter(entity => entity.kind === kind)
  .toSorted((left, right) => compareText(entityId(left), entityId(right))).map(entity => entity.value);
const reconstruct = (revision: number, documents: ManagedRecord[], hidden: Entity<HiddenKind>[], conversationId: string): Board => {
  const meta = values(hidden, 'meta');
  requirePolicy(meta.length === 1, 'missing_pages_board_meta');
  const header = object(meta[0]);
  requirePolicy(Object.keys(header).length === 5 && header.conversationId === conversationId
    && 'concurrencyLimit' in header && 'checkpoint' in header && 'checkinMinutes' in header && validRefs([header.history as FileRef]), 'invalid_pages_board_meta');
  const entities: Entity[] = [...documents.map(record => record.data), ...hidden];
  return validateBoard({ ...header, revision, tasks: values(entities, 'task'), asks: values(entities, 'ask'), rules: values(entities, 'rule'),
    runs: values(entities, 'run'), outbox: values(entities, 'outbox') }, conversationId);
};
const readable = (value: unknown): string => {
  if (value === null) return 'none';
  if (Array.isArray(value)) return value.map(item => `- ${readable(item)}`).join('\n') || 'none';
  if (typeof value === 'object') return Object.entries(object(value)).filter(([, item]) => item !== undefined)
    .map(([key, item]) => `${key.replace(/([a-z])([A-Z])/g, '$1 $2')}: ${readable(item)}`).join('\n');
  return String(value);
};
const documentFor = (id: string, data: Entity<DocumentKind>): Document => {
  const value = object(data.value);
  const hasTitle = typeof value.title === 'string';
  return { title: hasTitle ? value.title as string : `Chief ${data.kind}: ${entityId(data)}`,
    blocks: [{ id: `${id}-body`, kind: 'paragraph', text: `Chief-managed record. Change work through Chief in shared Chat; use comments for answers. Direct body edits are not authorized.\n\n${readable(value)}`, marks: [] }] };
};

// -- Atomic per-document and per-chunk diff ----------------------------------
const changesFor = (pageId: string, before: Snapshot, next: Board): { changes: DocumentChange[]; state_changes: StateChange[] } => {
  const previous = new Map(before.records.map(record => [record.recordId, record.data]));
  const documents = documentsFor(next);
  requirePolicy(documents.length <= RECORD_LIMIT, 'pages_record_capacity');
  const current = new Map(documents.map(data => [recordId(pageId, data), data]));
  requirePolicy(current.size === documents.length, 'duplicate_pages_record_identity');
  const upserts: DocumentChange[] = [...current].filter(([id, data]) => !previous.has(id) || !same(previous.get(id), data)).map(([id, data]) => {
    requirePolicy(bytes(data) <= DATA_BYTES, 'pages_record_data_too_large');
    return { upsert: { record_id: id, data, document: documentFor(id, data) } };
  });
  const changes: DocumentChange[] = [...upserts, ...[...previous.keys()].filter(id => !current.has(id)).map(id => ({ delete: { record_id: id } }))];
  const oldStates = new Map(before.states.map(row => [row.key, row.value]));
  const newStates = new Map(statesFor(pageId, next).map(row => [row.key, row.value]));
  const puts: StateChange[] = [...newStates].filter(([key, value]) => !oldStates.has(key) || !same(oldStates.get(key), value)).map(([key, value]) => ({ put: { key, value } }));
  const state_changes: StateChange[] = [...puts, ...[...oldStates.keys()].filter(key => !newStates.has(key)).map(key => ({ delete: { key } }))];
  requirePolicy(changes.length + state_changes.length <= CHANGE_LIMIT, 'pages_atomic_change_limit');
  return { changes, state_changes };
};

// -- Stable current-state enumeration, never a retry loop --------------------
export const createNetworkPages = (rpc: NetworkPagesRpc, pageId: string, conversationId: string): PagesAdapter => {
  const query = (input: Record<string, unknown>, signal?: AbortSignal): Promise<unknown> => Promise.resolve()
    .then(() => { signal?.throwIfAborted(); return rpc.query('pages', input, signal); });
  const collection = (signal?: AbortSignal): Promise<Collection> => Promise.resolve()
    .then(() => query({ record_collection: { page_id: pageId } }, signal))
    .then(raw => {
      const value = object(raw).record_collection;
      requirePolicy(value !== null, 'record_collection_not_found');
      const header = object(value);
      requirePolicy(header.page_id === pageId, 'invalid_pages_collection_identity');
      const count = integer(header.record_count);
      requirePolicy(count <= RECORD_LIMIT, 'pages_record_capacity');
      return { revision: integer(header.revision), count };
    });
  const state = (key: string, revision: number, signal?: AbortSignal): Promise<StoredState | null> => Promise.resolve()
    .then(() => query({ record_state: { page_id: pageId, key } }, signal))
    .then(raw => {
      const value = object(raw).record_state;
      if (value === null) return null;
      const row = object(value);
      requirePolicy(row.key === key, 'invalid_pages_state_identity');
      const at = integer(row.revision);
      requirePolicy(at > 0 && bytes(row.value) <= STATE_BYTES, 'invalid_pages_state_chunk');
      requirePolicy(at <= revision, 'revision_conflict');
      return { key, value: row.value, revision: at };
    });
  const requiredState = (key: string, revision: number, signal?: AbortSignal): Promise<StoredState> => Promise.resolve()
    .then(() => state(key, revision, signal))
    .then(row => { requirePolicy(row !== null, 'missing_pages_state_chunk'); return row!; });
  const readHidden = (row: StoredState, revision: number, signal?: AbortSignal): Promise<{ states: StoredState[]; hidden: Entity<HiddenKind>[] }> => Promise.resolve()
    .then(() => {
      const header = parseHeader(row, revision);
      return Promise.all(Array.from({ length: header.parts }, (_, part) => requiredState(indexKey(part), revision, signal)))
        .then(index => ({ header, index, entries: parseManifest(decodeChunks(index, 'index', INDEX_KEY, header.parts), header) }));
    })
    .then(({ index, entries }) => Promise.all(entries.map(entry => Promise.resolve()
      .then(() => Promise.all(Array.from({ length: entry.parts }, (_, part) => requiredState(hiddenKey(pageId, entry.kind, entry.entityId, part), revision, signal))))
      .then(rows => {
        const value = decodeJson(decodeChunks(rows, entry.kind, entry.entityId, entry.parts));
        object(value);
        const entity = { kind: entry.kind, value };
        requirePolicy(entityId(entity) === entry.entityId, 'invalid_pages_state_identity');
        return { rows, entity };
      }))).then(groups => ({ states: [row, ...index, ...groups.flatMap(group => group.rows)], hidden: groups.map(group => group.entity) })));
  const enumerate = (header: Collection, after: string | null, prior: ManagedRecord[], signal?: AbortSignal): Promise<ManagedRecord[]> => Promise.resolve()
    .then(() => query({ records: { page_id: pageId, after, limit: PAGE_LIMIT } }, signal))
    .then(raw => {
      const page = object(object(raw).records);
      requirePolicy(integer(page.revision) === header.revision, 'revision_conflict');
      requirePolicy(Array.isArray(page.records) && page.records.length <= PAGE_LIMIT, 'invalid_pages_record_window');
      const records = (page.records as unknown[]).map(item => {
        const record = object(item);
        const data = object(record.data);
        requirePolicy(typeof data.kind === 'string' && documentKinds.has(data.kind)
          && Object.keys(data).length === 2 && 'value' in data && bytes(data) <= DATA_BYTES, 'invalid_pages_record_kind');
        const typed = data as unknown as Entity<DocumentKind>;
        const id = string(record.record_id);
        requirePolicy(id === recordId(pageId, typed), 'invalid_pages_record_identity');
        const at = integer(record.revision);
        requirePolicy(at > 0 && at <= header.revision, 'invalid_pages_record_revision');
        return { recordId: id, data: typed, revision: at };
      });
      const ordered = records.every((record, index) => {
        const previous = index === 0 ? after : records[index - 1].recordId;
        return previous === null || record.recordId > previous;
      });
      requirePolicy(ordered, 'invalid_pages_record_cursor');
      const all = [...prior, ...records];
      requirePolicy(all.length <= header.count, 'invalid_pages_record_count');
      if (page.next_after === null) { requirePolicy(all.length === header.count, 'invalid_pages_record_count'); return all; }
      const cursor = string(page.next_after);
      const progresses = records.length > 0 && cursor === records.at(-1)?.recordId && all.length < header.count;
      requirePolicy(progresses, 'invalid_pages_record_cursor');
      return enumerate(header, cursor, all, signal);
    });
  const snapshot = (signal?: AbortSignal): Promise<Snapshot> => Promise.resolve()
    .then(() => collection(signal))
    .then(header => state(INDEX_KEY, header.revision, signal).then((row): Snapshot | Promise<Snapshot> => {
      if (row === null) {
        requirePolicy(header.revision === 0 && header.count === 0, 'missing_pages_index');
        return { collection: header, records: [] as ManagedRecord[], states: [] as StoredState[], board: emptyBoard(conversationId) };
      }
      return readHidden(row, header.revision, signal)
        .then(({ states, hidden }) => enumerate(header, null, [], signal)
          .then(records => ({ collection: header, records, states, board: reconstruct(header.revision, records, hidden, conversationId) })));
    }))
    .then(snapshot => collection(signal).then(current => {
      requirePolicy(current.revision === snapshot.collection.revision, 'revision_conflict');
      requirePolicy(current.count === snapshot.collection.count, 'invalid_pages_record_count');
      return snapshot;
    }));
  const receipt = (operationId: string, signal?: AbortSignal): Promise<CheckedReceipt | null> => Promise.resolve()
    .then(() => {
      requirePolicy(validId(operationId) && operationId.length <= 160, 'invalid_operation_id');
      return query({ record_receipt: { page_id: pageId, request_id: requestId(pageId, operationId) } }, signal);
    })
    .then(raw => parseReceipt(raw, pageId, operationId));

  // -- State, documents and artifact ownership share the same native CAS -----
  const commit: PagesAdapter['commit'] = (request, signal) => Promise.resolve()
    .then(() => structuredClone(request))
    .then(input => snapshot(signal).then(before => ({ input, before })))
    .then(({ input, before }): Promise<CommitReceipt> | CommitReceipt => {
      const stale = before.collection.revision !== input.expectedRevision;
      if (stale) return { kind: 'conflict', revision: before.collection.revision };
      const next = validateBoard(input.value, conversationId);
      requirePolicy(Number.isSafeInteger(input.expectedRevision + 1) && next.revision === input.expectedRevision + 1, 'record_revision_mismatch');
      const metadata = parseMetadata(input.metadata, input.requestId);
      requirePolicy(same(next.history, metadata.history), 'history_head_mismatch');
      const artifacts = artifactHashes(input.artifacts, metadata);
      const wireRequestId = requestId(pageId, input.requestId);
      const payload = { commit_records: { page_id: pageId, expected_revision: input.expectedRevision, request_id: wireRequestId,
        ...changesFor(pageId, before, next), metadata, artifacts } };
      requirePolicy(bytes(payload) <= BATCH_BYTES, 'pages_record_batch_too_large');
      const expectedDigest = [...createHash('sha256').update(rustValueJson(payload), 'utf8').digest()];
      return Promise.resolve()
        .then(() => { signal?.throwIfAborted(); return rpc.submit('pages', payload, wireRequestId, signal); })
        .then(() => receipt(input.requestId, signal))
        .then(confirmed => {
          requirePolicy(confirmed !== null, 'invalid_commit_receipt');
          const { revision, ...recordedMetadata } = confirmed!.receipt;
          const digestMatches = confirmed!.digest.every((byte, index) => byte === expectedDigest[index]);
          const rootsMatch = same(confirmed!.artifacts, artifacts);
          requirePolicy(revision === next.revision && same(recordedMetadata, metadata) && digestMatches && rootsMatch, 'invalid_commit_receipt');
          return { kind: 'committed' as const, requestId: input.requestId, revision: next.revision };
        });
    });
  return {
    read: (signal): Promise<RecordSnapshot> => Promise.resolve().then(() => snapshot(signal)).then(current => ({ revision: current.collection.revision, value: current.board })),
    receipt: (operationId, signal): Promise<OperationReceipt | null> => Promise.resolve().then(() => receipt(operationId, signal)).then(value => value?.receipt ?? null),
    commit,
  };
};
