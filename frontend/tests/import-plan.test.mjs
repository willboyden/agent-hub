import test from 'node:test';
import assert from 'node:assert/strict';
import { defaultChoice, defaultChoices, defaultSelection, previewImport, importItems, importSummary, conflictCount, setAll, itemKey } from '../js/import-plan.js';

const items = [
  { kind: 'skill', name: 'new', conflict: null },
  { kind: 'skill', name: 'same', conflict: { existing_source: 'local', equal: true, differences: [] } },
  { kind: 'skill', name: 'diff', conflict: { existing_source: 'imported:x', equal: false, differences: ['body differs'] } },
];
test('defaults: new -> import, equal -> link, different -> skip (nothing is overwritten unasked)', () => {
  assert.deepEqual(items.map(defaultChoice), ['import', 'link', 'skip']);
  assert.equal(defaultSelection(items).size, 3);
  assert.deepEqual(defaultChoices(items), { 'skill/new': 'import', 'skill/same': 'link', 'skill/diff': 'skip' });
});
test('preview follows per-item choices; rename shows -2; only conflicting items carry on_conflict in the request', () => {
  const sel = defaultSelection(items);
  const ch = { ...defaultChoices(items), 'skill/diff': 'rename' };
  const p = previewImport(items, sel, ch);
  assert.deepEqual(p.map((x) => [x.action, x.to]), [['import', 'new'], ['link', 'same'], ['rename', 'diff-2']]);
  assert.deepEqual(importItems(p), [{ kind: 'skill', name: 'new' }, { kind: 'skill', name: 'same', on_conflict: 'link' }, { kind: 'skill', name: 'diff', on_conflict: 'rename' }]);
  assert.equal(previewImport(items, new Set(['skill/new']), ch).length, 1);
});
test('summary, conflict count and bulk apply only touch selected conflicts', () => {
  const sel = defaultSelection(items);
  assert.deepEqual(importSummary(previewImport(items, sel, defaultChoices(items))), { import: 1, link: 1, skip: 1, rename: 0, replace: 0, total: 3, writes: 1, links: 1 });
  assert.equal(conflictCount(items, sel), 2); assert.equal(conflictCount(items, new Set(['skill/new'])), 0);
  assert.deepEqual(setAll(items, sel, 'replace'), { 'skill/same': 'replace', 'skill/diff': 'replace' });
  assert.deepEqual(setAll(items, new Set([itemKey(items[2])]), 'link'), { 'skill/diff': 'link' });
});
