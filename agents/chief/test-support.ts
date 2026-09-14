// Deterministic authoritative adapters. Failure switches model lost replies
// after durability, never timing; tests synchronize on explicit deferred events.
import { createHash } from 'node:crypto';

import type { Board, ChiefAdapters, CommitReceipt, EffectReceipt, OperationMetadata, OutboxPayload, TaskSpec } from './contracts.ts';
import { emptyBoard } from './domain.ts';
import { fingerprint } from './store.ts';

export const conversationId = 'chief-conversation';
export const task = (id = 'task-1', dependencies: string[] = []): TaskSpec => ({ id, key: id, title: 'Implement authorized work', brief: 'Implement exactly the requested change.', scope: ['src'], access: 'write', dependencies });
export const ref = { fileId: 'file-evidence', hash: 'a'.repeat(64) };
export const memory = (): ChiefAdapters & {
  state: { board: Board; loseCommit: boolean; failCommit: boolean; loseEffect: boolean; failEffect: boolean; holdControl: boolean };
  receipts: Map<string, EffectReceipt>; submissions: { operationId: string; payload: OutboxPayload }[];
  fileWrites: Map<string, string>; retained: Set<string>;
} => {
  const state = { board: emptyBoard(conversationId), loseCommit: false, failCommit: false, loseEffect: false, failEffect: false, holdControl: false };
  const commits = new Map<string, { digest: string; receipt: CommitReceipt; metadata: OperationMetadata }>();
  const receipts = new Map<string, EffectReceipt>();
  const submissions: { operationId: string; payload: OutboxPayload }[] = [];
  const fileWrites = new Map<string, string>();
  const retained = new Set<string>();
  return {
    state, receipts, submissions, fileWrites, retained,
    pages: {
      read: async () => ({ revision: state.board.revision, value: structuredClone(state.board) }),
      receipt: async operationId => {
        const saved = commits.get(operationId);
        if (!saved || saved.receipt.kind !== 'committed') return null;
        return { ...structuredClone(saved.metadata), revision: saved.receipt.revision };
      },
      commit: async input => {
        if (state.failCommit) throw new Error('SECRET_transport_failed');
        const prior = commits.get(input.requestId);
        if (prior) {
          if (prior.digest !== fingerprint(input)) throw new Error('conflicting_request_id');
          return prior.receipt;
        }
        if (input.expectedRevision !== state.board.revision) return { kind: 'conflict', revision: state.board.revision };
        state.board = structuredClone(input.value);
        input.artifacts.forEach(ref => retained.add(ref.hash));
        const receipt: CommitReceipt = { kind: 'committed', requestId: input.requestId, revision: state.board.revision };
        commits.set(input.requestId, { digest: fingerprint(input), receipt, metadata: structuredClone(input.metadata) });
        if (state.loseCommit) { state.loseCommit = false; throw new Error('SECRET_lost_commit_receipt'); }
        return receipt;
      },
    },
    files: { project: async ref => ref, read: async ref => {
      if (!retained.has(ref.hash)) throw new Error('artifact_not_retained');
      const content = fileWrites.get(ref.fileId);
      if (content === undefined) throw new Error('artifact_unavailable');
      return content;
    }, put: async input => {
      const old = fileWrites.get(input.operationId);
      if (old && old !== input.content) throw new Error('file_operation_reused');
      fileWrites.set(input.operationId, input.content);
      return { fileId: input.operationId, hash: createHash('sha256').update(input.content).digest('hex') };
    } },
    effects: {
      prepare: async (operationId, input) => {
        const payload = structuredClone(input);
        const sha = (value: string): string => createHash('sha256').update(value).digest('hex');
        const requestId = sha(operationId);
        const runId = 'memory-native-run';
        return { identity: { requestId, runId, target: payload.kind === 'wake' ? 'runs' : 'tasks',
          receiptId: `action/${sha(runId)}/${sha(requestId)}`, payloadFingerprint: fingerprint(payload) }, submit: async () => {
          submissions.push({ operationId, payload });
          if (state.failEffect) throw new Error('SECRET_submit_failed');
          const receipt = (() : EffectReceipt => {
            switch (payload.kind) {
              case 'dispatch': return { kind: 'dispatch', operationId, jobId: `job:${payload.runId}`, conversationId: payload.conversation.kind === 'continue' ? payload.conversation.conversationId : `conversation:${payload.runId}` };
              case 'control': return { kind: 'control', operationId, jobId: payload.jobId, status: 'applied' };
              case 'wake': return { kind: 'wake', operationId, inboxId: `inbox:${operationId}` };
            }
          })();
          if (payload.kind === 'control' && state.holdControl) throw new Error('pending_control_ack');
          receipts.set(operationId, receipt);
          if (state.loseEffect) { state.loseEffect = false; throw new Error('SECRET_lost_effect_receipt'); }
          return receipt;
        } };
      },
      lookup: async operationId => receipts.get(operationId) ?? null,
    },
  };
};
