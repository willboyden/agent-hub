import test from 'node:test';
import assert from 'node:assert/strict';
import { buildOverrides, visibleSuggestions, loadDismissed, saveDismissed } from '../js/suggest-logic.js';
import { normalizeClient, normalizeSuggestion } from '../js/adapt.js';

const S = [{ id: 'a', title: 'A', members: [{ kind: 'skill', name: 'x' }, { kind: 'skill', name: 'y' }] }, { id: 'b', title: 'B', members: [{ kind: 'skill', name: 'z' }] }];
test('overrides carry only what the user changed: a new title and/or the trimmed member list', () => {
  assert.deepEqual(buildOverrides({}, S), {});
  assert.deepEqual(buildOverrides({ a: { title: 'A', removed: new Set() } }, S), {});
  assert.deepEqual(buildOverrides({ a: { title: ' Renamed ', removed: new Set(['skill/y']) } }, S), { a: { title: 'Renamed', members: [{ kind: 'skill', name: 'x' }] } });
  assert.deepEqual(buildOverrides({ b: { title: '  ', removed: new Set() } }, S), {}, 'a blank title is not an override');
});
test('dismissals are remembered by id and a broken store is tolerated', () => {
  const mem = new Map(); const store = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => mem.set(k, v) };
  saveDismissed(new Set(['a']), store); const d = loadDismissed(store);
  assert.deepEqual(visibleSuggestions(S, d).map((x) => x.id), ['b']);
  assert.equal(loadDismissed({ getItem() { throw new Error('x'); } }).size, 0); assert.equal(saveDismissed(new Set(), { setItem() { throw new Error('x'); } }), false);
  assert.equal(loadDismissed({ getItem: () => 'not json' }).size, 0);
});
test('suggestion and client status normalisation (new statuses, old "drift" mapped, drift detail lists)', () => {
  assert.deepEqual(normalizeSuggestion({ id: 'i', members: [{ kind: 'skill', name: 'n', junk: 1 }] }).members, [{ kind: 'skill', name: 'n' }]);
  const c = normalizeClient({ id: 'c', status: 'edited_outside', drift: { edited_outside: [{ root: 'r', path: 'p', reason: 'why' }], pending_changes: 2 }, adopted_at: 5, last_applied_at: null });
  assert.deepEqual([c.status, c.drift.edited_outside[0].reason, c.drift.pending_changes, c.adopted_at, c.last_applied_at], ['edited_outside', 'why', 2, 5, null]);
  assert.equal(normalizeClient({ id: 'c', status: 'drift' }).status, 'edited_outside');
  assert.deepEqual(normalizeClient({ id: 'c', status: 'in_sync' }).drift, { edited_outside: [], pending_changes: 0 });
});
