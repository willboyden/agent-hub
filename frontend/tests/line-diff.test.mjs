import test from 'node:test';
import assert from 'node:assert/strict';
import { splitLines, diffLines, opStats as diffStats, toUnified, hunkHeader, unifiedText } from '../js/line-diff.js';
import { parseUnified } from '../js/diff.js';

const apply = (ops) => ({ a: ops.filter((o) => o.type !== 'add').map((o) => o.text), b: ops.filter((o) => o.type !== 'del').map((o) => o.text) });

test('splitLines ignores one trailing newline and normalises CRLF', () => {
  assert.deepEqual(splitLines('a\nb\n'), ['a', 'b']); assert.deepEqual(splitLines('a\r\nb'), ['a', 'b']); assert.deepEqual(splitLines(''), []); assert.deepEqual(splitLines('\n'), ['']);
});
test('identical text yields only eq ops and no change', () => {
  const ops = diffLines('a\nb\nc\n', 'a\nb\nc'); assert.ok(ops.every((o) => o.type === 'eq')); assert.equal(diffStats(ops).changed, false);
});
test('single line change is one del + one add with correct line numbers', () => {
  const ops = diffLines('a\nb\nc', 'a\nX\nc');
  assert.deepEqual(ops.map((o) => [o.type, o.text, o.a, o.b]), [['eq', 'a', 1, 1], ['del', 'b', 2, undefined], ['add', 'X', undefined, 2], ['eq', 'c', 3, 3]]);
  assert.deepEqual(diffStats(ops), { add: 1, del: 1, changed: true });
});
test('pure insertion, deletion, empty old and empty new', () => {
  assert.deepEqual(diffStats(diffLines('a\nc', 'a\nb\nc')), { add: 1, del: 0, changed: true });
  assert.deepEqual(diffStats(diffLines('a\nb\nc', 'a\nc')), { add: 0, del: 1, changed: true });
  assert.deepEqual(diffStats(diffLines('', 'x\ny')), { add: 2, del: 0, changed: true });
  assert.deepEqual(diffStats(diffLines('x\ny', '')), { add: 0, del: 2, changed: true });
});
test('reconstruction property over pseudo-random edits (old and new are recoverable from ops) and the script is minimal-ish', () => {
  let s = 12345; const r = (n) => { s = (s * 1103515245 + 12345) & 0x7fffffff; return s % n; };
  for (let k = 0; k < 60; k++) {
    const A = Array.from({ length: r(30) }, () => `l${r(8)}`);
    const B = A.filter(() => r(4) !== 0); for (let i = 0; i < r(6); i++) B.splice(r(B.length + 1), 0, `n${r(8)}`);
    const ops = diffLines(A.join('\n'), B.join('\n')), got = apply(ops);
    assert.deepEqual(got.a, A); assert.deepEqual(got.b, B);
    // never worse than delete-all + add-all
    assert.ok(diffStats(ops).add + diffStats(ops).del <= A.length + B.length);
  }
});
test('Myers finds the shortest script for a classic example', () => {
  const ops = diffLines('A\nB\nC\nA\nB\nB\nA', 'C\nB\nA\nB\nA\nC');
  assert.equal(diffStats(ops).add + diffStats(ops).del, 5);
});
test('toUnified: context lines, separate hunks, headers', () => {
  const a = Array.from({ length: 30 }, (_, i) => `line${i + 1}`), b = [...a]; b[2] = 'CHANGED'; b[25] = 'ALSO';
  const hunks = toUnified(diffLines(a.join('\n'), b.join('\n')), 3);
  assert.equal(hunks.length, 2);
  assert.equal(hunkHeader(hunks[0]), '@@ -1,6 +1,6 @@'); assert.equal(hunks[0].lines.filter((l) => l.type === 'del').length, 1);
  assert.equal(hunkHeader(hunks[1]), '@@ -23,7 +23,7 @@');
  assert.deepEqual(toUnified(diffLines('a', 'a')), []);
});
test('large near-identical files stay fast (prefix/suffix trimming)', () => {
  const a = Array.from({ length: 20000 }, (_, i) => `k${i} = ${i}`); const b = [...a]; b[10000] = 'k10000 = changed';
  const t0 = Date.now(); const ops = diffLines(a.join('\n'), b.join('\n')); assert.ok(Date.now() - t0 < 1500); assert.equal(diffStats(ops).add, 1);
});

test('unifiedText is a unified diff the plan renderer (diff.js) reads back line for line', () => {
  assert.equal(unifiedText('a\nb', 'a\nb\n'), '');
  const u = unifiedText('a\nb\nc\n', 'a\nX\nc\n');
  assert.equal(u, '--- loaded\n+++ draft\n@@ -1,3 +1,3 @@\n a\n-b\n+X\n c\n');
  assert.deepEqual(parseUnified(u).map((l) => [l.type, l.text]), [['meta', '--- loaded'], ['meta', '+++ draft'], ['hunk', '@@ -1,3 +1,3 @@'], ['ctx', 'a'], ['del', 'b'], ['add', 'X'], ['ctx', 'c']]);
});
