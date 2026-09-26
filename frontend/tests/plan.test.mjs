import test from 'node:test';
import assert from 'node:assert/strict';
import { isBlocked, applyTotals, canApply, stale, summarizeChanges, suggestMessage, validMessage, fileCounts } from '../js/plan.js';

const f = (action, path = 'p') => ({ action, path });
const plan = { content_hash: 'h1', clients: [
  { client: 'a', floor_violations: [], files: [f('add'), f('change'), f('unchanged'), f('conflict', 'c.md')], summary: { add: 1, change: 1, unchanged: 1, conflict: 1 } },
  { client: 'b', floor_violations: [{ code: 'x' }], files: [f('add')] },
  { client: 'c', blocked: true, files: [f('remove')] },
  { client: 'd', floor_violations: [], files: [f('advisory'), f('unchanged')] },
] };

test('blocked = server flag or any floor violation', () => assert.deepEqual(plan.clients.map(isBlocked), [false, true, true, false]));
test('applyTotals counts only writable files of unblocked clients and names the blocked ones', () => {
  assert.deepEqual(applyTotals(plan), { clients: 2, blocked: 2, blockedIds: ['b', 'c'], files: 2, conflicts: 1 });
  assert.equal(applyTotals(plan, new Set(['b'])).clients, 0);
});
test('canApply needs the review tick, something to do, and at least one unblocked client', () => {
  assert.deepEqual(canApply(plan, { reviewed: false }), { ok: false, reason: 'not_reviewed' });
  assert.equal(canApply(plan, { reviewed: true }).ok, true);
  assert.equal(canApply({ clients: [plan.clients[1], plan.clients[2]] }, { reviewed: true }).reason, 'all_blocked');
  assert.equal(canApply({ clients: [plan.clients[3]] }, { reviewed: true }).reason, 'nothing_to_do');
  assert.equal(canApply({ clients: [{ client: 'x', files: [f('conflict', 'c')] }] }, { reviewed: true }).reason, 'nothing_to_do');
  assert.equal(canApply({ clients: [{ client: 'x', files: [f('conflict', 'c')] }] }, { reviewed: true, adopt: new Set(['c']) }).ok, true, 'adopting a conflict counts as work');
  assert.equal(canApply({ clients: [] }, { reviewed: true }).reason, 'no_clients');
});
test('stale compares content hashes', () => { assert.equal(stale(plan, 'h1'), false); assert.equal(stale(plan, 'h2'), true); assert.equal(stale(null, 'h'), false); });
test('fileCounts prefers the server summary, else counts', () => { assert.equal(fileCounts(plan.clients[0]).add, 1); assert.deepEqual(fileCounts({ files: [f('add'), f('add')] }), { add: 2 }); });

test('pending-change summary and commit message suggestion', () => {
  const files = [{ path: 'skills/a/SKILL.md' }, { path: 'skills/a/hub.yaml' }, { path: 'skills/b/SKILL.md' }, { path: 'profiles/claude.yaml' }];
  assert.deepEqual(summarizeChanges(files), [{ dir: 'skills', count: 2 }, { dir: 'profiles', count: 1 }]);
  assert.equal(suggestMessage(files), 'Update 2 skills, 1 profile');
  assert.equal(suggestMessage([]), '');
  assert.equal(validMessage('  ab '), false); assert.equal(validMessage('abc'), true); assert.equal(validMessage('x'.repeat(201)), false);
});
