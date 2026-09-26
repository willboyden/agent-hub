import test from 'node:test';
import assert from 'node:assert/strict';
import { applySelection, pruneSelection, selectAll } from '../js/select.js';

const order = ['a', 'b', 'c', 'd', 'e'];
const S = (ids = [], anchor = null) => ({ ids: new Set(ids), anchor });

test('plain click selects only that one; clicking the sole selection clears it', () => {
  let s = applySelection(S(['a', 'b']), order, 'c');
  assert.deepEqual([...s.ids], ['c']);
  s = applySelection(s, order, 'c');
  assert.equal(s.ids.size, 0);
});

test('ctrl/meta toggles and moves the anchor', () => {
  let s = applySelection(S(), order, 'a', { ctrl: true });
  s = applySelection(s, order, 'c', { ctrl: true });
  assert.deepEqual([...s.ids].sort(), ['a', 'c']);
  s = applySelection(s, order, 'a', { ctrl: true });
  assert.deepEqual([...s.ids], ['c']);
  assert.equal(s.anchor, 'a');
});

test('shift selects the range from the anchor in either direction and keeps earlier picks', () => {
  let s = applySelection(S(), order, 'b', { ctrl: true });
  s = applySelection(s, order, 'd', { shift: true });
  assert.deepEqual([...s.ids].sort(), ['b', 'c', 'd']);
  const r = applySelection(applySelection(S(), order, 'd', { ctrl: true }), order, 'b', { shift: true });
  assert.deepEqual([...r.ids].sort(), ['b', 'c', 'd']);
  const keep = applySelection(S(['e'], 'b'), order, 'c', { shift: true });
  assert.deepEqual([...keep.ids].sort(), ['b', 'c', 'e']);
});

test('shift without an anchor behaves like a plain click', () => {
  const s = applySelection(S(), order, 'c', { shift: true });
  assert.deepEqual([...s.ids], ['c']);
});

test('range only covers currently visible ids', () => {
  const s = applySelection(S(['a'], 'a'), ['a', 'c', 'e'], 'e', { shift: true });
  assert.deepEqual([...s.ids].sort(), ['a', 'c', 'e']);
});

test('pruneSelection drops ids that are no longer visible; selectAll', () => {
  const p = pruneSelection(S(['a', 'z'], 'z'), order);
  assert.deepEqual([...p.ids], ['a']); assert.equal(p.anchor, null);
  assert.equal(selectAll(order).ids.size, 5);
});
