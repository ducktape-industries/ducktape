// Managed Page discussion admissions are frozen by Runs. Attribution's actual
// mutator, never a comment's persistent original author, supplies reply identity.
import assert from 'node:assert/strict';
import test from 'node:test';

import { prepareChiefEvents } from './inputs.ts';
import type { CommittedConversationEvent, ManagedAskContext } from './inputs.ts';
import { createChiefService } from './service.ts';
import { conversationId, memory } from './test-support.ts';

const managed: ManagedAskContext = { collectionPageId: 'board', matchesAskTarget: (askId, target) => askId === 'ask-1' && ['ask-page', 'ask-body'].includes(target) };
const comment = (revision = 1, actor = 42, text = 'Use the safer option.'): CommittedConversationEvent => ({
  sequence: revision, admitted_at: 1, operation_id: `attr-${revision}`, actor: { Module: 'pages' },
  input: { event: { kind: 'attribution', content: {
    attribution: { revision, actor: { account: actor }, reason: { defined: 'managed_record_comment' }, source: { module: 'pages', kind: 'comment', object: 'comment-1' } },
    source: { mutation: revision === 1 ? 'created' : 'edited', collection_page_id: 'board', page_id: 'ask-page',
      comment: { id: 'comment-1', thread_id: 'thread-1', author: { account: 7 }, text, mentions: [], created_at: 1, edited_at: revision === 1 ? null : 2, deleted: false },
      thread: { id: 'thread-1', target: 'ask-body', opener: { account: 7 }, created_at: 1, anchor: null, resolved: false, resolved_by: null } },
  } } },
});
const fixture = async () => {
  const adapters = memory();
  const service = createChiefService(adapters, conversationId);
  const result = await service.execute({ kind: 'change', operationId: 'ask', expectedRevision: 0, action: { kind: 'ask_open', ask: {
    id: 'ask-1', key: 'budget', title: 'Choose a budget', question: 'Approve the charge?', whyMember: 'Money is involved', ifUnasked: 'Charge is irreversible', recommendation: 'Wait', options: [], artifacts: [], blocks: [], sources: [], addressedTo: ['acct:42'],
  } } });
  assert.equal(result.success, true);
  return { adapters, service, read: async () => structuredClone(adapters.state.board) };
};

test('plain fresh reply saves exact attributed member without a recursive inbox wake', async () => {
  const { adapters, service, read } = await fixture();
  const prepared = await prepareChiefEvents(service, read, [comment()], undefined, managed);
  assert.equal(prepared.wake, true);
  assert.equal(adapters.state.board.asks[0].status, 'replied');
  assert.deepEqual(adapters.state.board.asks[0].reply?.source, { kind: 'page_comment', memberId: 'acct:42', conversationId, messageId: 'comment-1' });
  assert.equal(adapters.state.board.asks[0].reply?.text, 'Use the safer option.');
  assert.equal(adapters.submissions.length, 0);
  const revision = adapters.state.board.revision;
  await prepareChiefEvents(service, read, [comment()], undefined, managed);
  assert.equal(adapters.state.board.revision, revision);
});

test('edits never become fresh approval and never retroactively rewrite saved decisions', async () => {
  const { adapters, service, read } = await fixture();
  await prepareChiefEvents(service, read, [comment(2, 42, 'Edited approval')], undefined, managed);
  assert.equal(adapters.state.board.asks[0].status, 'open');
  await prepareChiefEvents(service, read, [comment()], undefined, managed);
  await prepareChiefEvents(service, read, [comment(3, 99, 'Different member edited original author text')], undefined, managed);
  assert.equal(adapters.state.board.asks[0].reply?.text, 'Use the safer option.');
  assert.equal(adapters.state.board.asks[0].reply?.source.memberId, 'acct:42');
});

test('wrong actual member, spoofed event and wrong collection cannot impersonate an answer', async () => {
  const { adapters, service, read } = await fixture();
  await prepareChiefEvents(service, read, [comment(1, 7)], undefined, managed);
  assert.equal(adapters.state.board.asks[0].status, 'open');
  const spoof = await prepareChiefEvents(service, read, [{ ...comment(), actor: { External: [42] } }], undefined, managed);
  assert.match(spoof.prompt!, /quarantined_input.*untrusted_page_event/);
  const foreign = await prepareChiefEvents(service, read, [comment()], undefined, { ...managed, collectionPageId: 'other' });
  assert.match(foreign.prompt!, /quarantined_input.*wrong_comment_source/);
  assert.equal(adapters.state.board.asks[0].status, 'open');
});
