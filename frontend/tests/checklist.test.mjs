import test from 'node:test';
import assert from 'node:assert/strict';
import { checklist, rememberPlanViewed, readPlanViewed } from '../js/checklist.js';
import { isRedacted, stripRedacted, listOf } from '../js/data.js';
import { fmtBytes, timeAgo, debounce, readableText } from '../js/util.js';

test('checklist is derived from server truth: nothing applied means Apply is never done, however many plans were made', () => {
  let s = checklist({});
  assert.deepEqual(s.map((x) => x.id), ['import', 'organise', 'plan', 'apply']);
  assert.deepEqual(s.map((x) => x.next), [true, false, false, false]);
  s = checklist({ itemCount: 129, collectionCount: 0 });
  assert.deepEqual(s.map((x) => x.done), [true, false, false, false]); assert.equal(s[1].next, true);
  s = checklist({ itemCount: 129, collectionCount: 3, viewedHash: 'h1', contentHash: 'h1', appliedCount: 0 });
  assert.deepEqual(s.map((x) => x.done), [true, true, true, false]); assert.equal(s[3].next, true);
  s = checklist({ itemCount: 129, collectionCount: 3, viewedHash: 'old', contentHash: 'new', appliedCount: 1 });
  assert.deepEqual(s.map((x) => x.done), [true, true, false, true], 'a plan viewed for OLD content does not count as reviewed');
  assert.equal(checklist({ itemCount: 1, viewedHash: '', contentHash: '' })[2].done, false, 'empty hashes never match');
});

test('plan-viewed hash survives in storage and a throwing store is tolerated', () => {
  const mem = new Map(); const store = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => mem.set(k, v) };
  assert.equal(readPlanViewed(store), ''); assert.equal(rememberPlanViewed('abc', store), true); assert.equal(readPlanViewed(store), 'abc');
  const bad = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  assert.equal(rememberPlanViewed('x', bad), false); assert.equal(readPlanViewed(bad), '');
});

test('redacted secrets are recognised and never round-tripped', () => {
  assert.equal(isRedacted('[set]'), true); assert.equal(isRedacted('[set, 43 chars]'), true); assert.equal(isRedacted('ah_real'), false); assert.equal(isRedacted(null), false);
  assert.deepEqual(stripRedacted({ a: '[set]', b: 'x', c: '[set, 3 chars]', d: 1 }), { b: 'x', d: 1 });
  assert.deepEqual(listOf([1]), [1]); assert.deepEqual(listOf({ items: [2] }), [2]); assert.deepEqual(listOf(null), []);
});

test('util: bytes, relative time, debounce', async () => {
  assert.equal(fmtBytes(0), '0 B'); assert.equal(fmtBytes(1536), '1.5 KiB'); assert.equal(fmtBytes(5 * 2 ** 30), '5.0 GiB');
  assert.deepEqual(timeAgo(100, 130), { n: 30, u: 's' }); assert.equal(timeAgo(0, 100), ''); assert.deepEqual(timeAgo(1, 1 + 3 * 86400), { n: 3, u: 'd' });
  let n = 0; const d = debounce(() => n++, 10); d(); d(); d(); await new Promise((r) => setTimeout(r, 40)); assert.equal(n, 1);
});

test('readableText picks the higher-contrast colour on client colours (Okabe-Ito) and tolerates junk', () => {
  const lum = (hex) => { const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4)); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
  const cr = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };
  for (const bg of ['#0072b2', '#009e73', '#e69f00', '#cc79a7', '#56b4e9', '#d55e00', '#f0e442', '#a6a6a6']) assert.ok(cr(bg, readableText(bg)) >= 4.5, `${bg} -> ${readableText(bg)}`);
  assert.equal(readableText('nope'), '#0b0f14');
});
