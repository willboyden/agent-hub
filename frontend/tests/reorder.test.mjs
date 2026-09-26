import test from 'node:test';
import assert from 'node:assert/strict';
import { moveItem, moveRelative, moveBy, dropPosition, renumber, addMembers, memberKey } from '../js/reorder.js';

test('moveItem clamps and never mutates', () => {
  const a = ['a', 'b', 'c', 'd'];
  assert.deepEqual(moveItem(a, 0, 2), ['b', 'c', 'a', 'd']);
  assert.deepEqual(moveItem(a, 3, 0), ['d', 'a', 'b', 'c']);
  assert.deepEqual(moveItem(a, 1, 99), ['a', 'c', 'd', 'b']);
  assert.deepEqual(moveItem(a, 9, 0), a);
  assert.deepEqual(a, ['a', 'b', 'c', 'd']);
});

test('moveRelative places before/after the target, in either direction', () => {
  const a = ['a', 'b', 'c', 'd'];
  assert.deepEqual(moveRelative(a, 'a', 'c', 'before'), ['b', 'a', 'c', 'd']);
  assert.deepEqual(moveRelative(a, 'a', 'c', 'after'), ['b', 'c', 'a', 'd']);
  assert.deepEqual(moveRelative(a, 'd', 'a', 'before'), ['d', 'a', 'b', 'c']);
  assert.deepEqual(moveRelative(a, 'd', 'b', 'after'), ['a', 'b', 'd', 'c']);
});

test('moveRelative edge cases: same id, unknown ids', () => {
  const a = ['a', 'b'];
  assert.deepEqual(moveRelative(a, 'a', 'a'), a);
  assert.deepEqual(moveRelative(a, 'x', 'a'), a);
  assert.deepEqual(moveRelative(a, 'a', 'x'), a);
});

test('keyboard moveBy clamps at the ends', () => {
  const a = ['a', 'b', 'c'];
  assert.deepEqual(moveBy(a, 'b', -1), ['b', 'a', 'c']);
  assert.deepEqual(moveBy(a, 'a', -1), a);
  assert.deepEqual(moveBy(a, 'c', 1), a);
  assert.deepEqual(moveBy(a, 'a', 1), ['b', 'a', 'c']);
});

test('dropPosition uses the vertical midpoint', () => {
  assert.equal(dropPosition(100, 40, 110), 'before');
  assert.equal(dropPosition(100, 40, 130), 'after');
});

test('renumber gives steps of 10 in order', () => assert.deepEqual(renumber(['x', 'y']), [{ id: 'x', order: 10 }, { id: 'y', order: 20 }]));

test('addMembers appends only new (kind,name) pairs', () => {
  const cur = [{ kind: 'skill', name: 'a' }];
  const out = addMembers(cur, [{ kind: 'skill', name: 'a' }, { kind: 'agent', name: 'a' }, { kind: 'agent', name: 'a' }]);
  assert.deepEqual(out.map(memberKey), ['skill/a', 'agent/a']);
  assert.equal(cur.length, 1);
});
