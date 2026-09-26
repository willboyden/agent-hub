import test from 'node:test';
import assert from 'node:assert/strict';
import { classifyLine, parseUnified, diffStats } from '../js/diff.js';

test('classifyLine: headers before the first hunk, add/del/ctx inside', () => {
  assert.equal(classifyLine('--- a/x', false), 'meta');
  assert.equal(classifyLine('+++ b/x', false), 'meta');
  assert.equal(classifyLine('@@ -1,2 +1,2 @@'), 'hunk');
  assert.equal(classifyLine('+added'), 'add');
  assert.equal(classifyLine('-removed'), 'del');
  assert.equal(classifyLine(' same'), 'ctx');
  assert.equal(classifyLine('\\ No newline at end of file'), 'note');
});

test('inside a hunk, "--- x" is a DELETION of "-- x", not a header', () => {
  assert.equal(classifyLine('--- x', true), 'del');
  assert.equal(classifyLine('+++ x', true), 'add');
});

const SAMPLE = '--- a/f.md\n+++ b/f.md\n@@ -1,3 +1,3 @@\n keep\n-old\n+new\n tail\n';
test('parseUnified numbers old/new lines and strips the sign', () => {
  const l = parseUnified(SAMPLE);
  assert.deepEqual(l.map((x) => x.type), ['meta', 'meta', 'hunk', 'ctx', 'del', 'add', 'ctx']);
  assert.deepEqual(l.map((x) => [x.oldNo, x.newNo]), [[null, null], [null, null], [null, null], [1, 1], [2, null], [null, 2], [3, 3]]);
  assert.equal(l[4].text, 'old'); assert.equal(l[5].text, 'new');
});

test('new-file diff (from /dev/null) has only additions', () => {
  const d = '--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+a\n+b\n';
  assert.deepEqual(diffStats(d), { add: 2, del: 0 });
  assert.deepEqual(parseUnified(d).filter((x) => x.type === 'add').map((x) => x.newNo), [1, 2]);
});

test('markup in file content stays plain text data', () => {
  const l = parseUnified('@@ -1 +1 @@\n-<script>alert(1)</script>\n+<img src=x onerror=alert(1)>\n');
  assert.equal(l[1].text, '<script>alert(1)</script>');
  assert.equal(l[2].text, '<img src=x onerror=alert(1)>');
});

test('empty and CRLF input', () => {
  assert.deepEqual(parseUnified(''), []);
  assert.deepEqual(parseUnified(null), []);
  assert.equal(parseUnified('@@ -1 +1 @@\r\n-a\r\n+b\r\n').length, 3);
  assert.deepEqual(diffStats(SAMPLE), { add: 1, del: 1 });
});
