import test from 'node:test';
import assert from 'node:assert/strict';
import { cellKey, isToggleable, nextEnabled, displayState, stageToggle, stageMany, undo, emptyModel, stagedCount, bulkTarget, toBulkCalls, summarize } from '../js/matrix.js';

const on = { state: 'on' }, off = { state: 'off' }, via = { state: 'via_collection' }, uns = { state: 'unsupported', reason: 'no' }, blk = { state: 'blocked', reason: 'scan' };
const T = (client, name, kind = 'skill') => ({ client, kind, name });

test('only on/off/via_collection cells are toggleable; nextEnabled flips the effective state', () => {
  assert.deepEqual([on, off, via, uns, blk].map(isToggleable), [true, true, true, false, false]);
  assert.deepEqual([on, off, via, uns, blk].map(nextEnabled), [false, true, false, null, null]);
});

test('displayState shows staged value with a pending flag, otherwise the server cell', () => {
  const k = cellKey('c', 'skill', 'a');
  assert.deepEqual(displayState(off, {}, k), { state: 'off', reason: undefined, pending: false });
  assert.equal(displayState(off, { [k]: { enabled: true } }, k).state, 'on');
  assert.equal(displayState(off, { [k]: { enabled: true } }, k).pending, true);
  assert.equal(displayState(blk, {}, k).reason, 'scan');
});

test('stageToggle stages, toggling back removes the no-op entry, and each step is undoable', () => {
  let m = emptyModel();
  m = stageToggle(m, T('c', 'a'), off);
  assert.equal(stagedCount(m), 1);
  assert.equal(m.staged[cellKey('c', 'skill', 'a')].enabled, true);
  m = stageToggle(m, T('c', 'a'), off);          // back to the server value => nothing to save
  assert.equal(stagedCount(m), 0);
  m = undo(m); assert.equal(stagedCount(m), 1);   // undo the "toggle back"
  m = undo(m); assert.equal(stagedCount(m), 0);
  assert.equal(undo(m), m, 'undo on empty history is a no-op');
});

test('a via_collection cell toggles to an explicit OFF override', () => {
  const m = stageToggle(emptyModel(), T('c', 'a'), via);
  assert.equal(m.staged[cellKey('c', 'skill', 'a')].enabled, false);
});

test('non-toggleable cells are ignored and do not add undo steps', () => {
  const m = emptyModel();
  assert.equal(stageToggle(m, T('c', 'a'), uns), m);
  assert.equal(stageToggle(m, T('c', 'a'), blk), m);
});

test('stageMany is one undo step, skips non-toggleable and no-op cells', () => {
  const targets = [{ ...T('c1', 'a'), cell: off }, { ...T('c2', 'a'), cell: on }, { ...T('c3', 'a'), cell: uns }, { ...T('c4', 'a'), cell: blk }];
  const m = stageMany(emptyModel(), targets, true);
  assert.equal(stagedCount(m), 1, 'only the off cell changes');
  assert.equal(m.undo.length, 1);
  assert.equal(stageMany(m, targets, true), m, 'staging the same thing again changes nothing');
  assert.equal(stagedCount(undo(m)), 0);
});

test('bulkTarget: any OFF (after staging) => turn on; all on => turn off; nothing toggleable => null', () => {
  const ts = [{ ...T('c1', 'a'), cell: on }, { ...T('c2', 'a'), cell: off }];
  assert.equal(bulkTarget(ts, emptyModel()), true);
  assert.equal(bulkTarget([ts[0], { ...T('c2', 'a'), cell: via }], emptyModel()), false);
  assert.equal(bulkTarget([{ ...T('c', 'a'), cell: uns }], emptyModel()), null);
  const m = stageMany(emptyModel(), ts, true);
  assert.equal(bulkTarget(ts, m), false, 'after staging everything on, the next header click turns off');
});

test('toBulkCalls groups items with identical client sets and same enabled value', () => {
  let m = emptyModel();
  m = stageMany(m, [{ ...T('c1', 'a'), cell: off }, { ...T('c2', 'a'), cell: off }, { ...T('c1', 'b'), cell: off }, { ...T('c2', 'b'), cell: off }, { ...T('c1', 'c'), cell: on }], true);
  m = stageMany(m, [{ ...T('c1', 'c'), cell: on }], false);
  const calls = toBulkCalls(m.staged);
  const on2 = calls.find((c) => c.enabled && c.clients.length === 2);
  assert.deepEqual(on2.items.map((i) => i.name).sort(), ['a', 'b']);
  assert.deepEqual(calls.find((c) => !c.enabled), { clients: ['c1'], items: [{ kind: 'skill', name: 'c' }], enabled: false });
  assert.deepEqual(summarize(m.staged), { on: 4, off: 1, total: 5 });
});

test('the model is never mutated in place', () => {
  const m = emptyModel(); const before = JSON.stringify(m);
  stageToggle(m, T('c', 'a'), off); stageMany(m, [{ ...T('c', 'a'), cell: off }], true);
  assert.equal(JSON.stringify(m), before);
});
