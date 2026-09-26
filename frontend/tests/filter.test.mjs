import test from 'node:test';
import assert from 'node:assert/strict';
import { filterItems, sortItems, searchScore, facets, itemId, activeFilterCount } from '../js/filter.js';

const items = [
  { kind: 'skill', name: 'code-review', title: 'code-review', description: 'Review a diff for bugs', tags: ['review', 'git'], groups: ['everyday'], source: 'local', issues: [], enabled_for: ['claude'], updated_at: 3 },
  { kind: 'skill', name: 'docker-lint', title: 'docker-lint', description: 'Lint compose files', tags: ['docker'], groups: ['ops'], source: 'imported:hermes', issues: [{ severity: 'warn' }], enabled_for: [], updated_at: 5 },
  { kind: 'agent', name: 'reviewer', title: 'reviewer', description: 'Reads diffs', tags: [], groups: ['everyday', 'ops'], source: 'local', issues: [], enabled_for: ['claude', 'opencode'], updated_at: 1 },
  { kind: 'mcp', name: 'github', title: 'github', description: 'Issues and PRs', tags: [], groups: [], source: 'local', issues: [{ severity: 'error' }], enabled_for: ['opencode'], updated_at: 4 },
];
const names = (l) => l.map((i) => i.name);

test('search is fuzzy, matches every word somewhere, and ranks name hits first', () => {
  assert.deepEqual(names(filterItems(items, { q: 'cvw' })), ['code-review']);
  assert.equal(searchScore('zzz', items[0]), -1);
  assert.deepEqual(names(filterItems(items, { q: 'lint compose' })), ['docker-lint'], 'words may match name and description');
  const ranked = sortItems(filterItems(items, { q: 'review' }), 'relevance', 'review');
  assert.deepEqual(names(ranked).slice(0, 2).sort(), ['code-review', 'reviewer'], 'name matches outrank description-only matches');
});

test('filters: kind, collection, tag, source, issues', () => {
  assert.deepEqual(names(filterItems(items, { kind: 'agent' })), ['reviewer']);
  assert.deepEqual(names(filterItems(items, { collection: 'ops' })), ['docker-lint', 'reviewer']);
  assert.deepEqual(names(filterItems(items, { tag: 'git' })), ['code-review']);
  assert.deepEqual(names(filterItems(items, { source: 'imported:hermes' })), ['docker-lint']);
  assert.deepEqual(names(filterItems(items, { issues: '1' })), ['docker-lint', 'github']);
  assert.deepEqual(names(filterItems(items, { issues: '0' })), ['code-review', 'reviewer']);
});

test('enabled filter with and without a client', () => {
  assert.deepEqual(names(filterItems(items, { client: 'claude' })), ['code-review', 'reviewer']);
  assert.deepEqual(names(filterItems(items, { client: 'claude', enabled: 'on' })), ['code-review', 'reviewer']);
  assert.deepEqual(names(filterItems(items, { client: 'claude', enabled: 'off' })), ['docker-lint', 'github']);
  assert.deepEqual(names(filterItems(items, { enabled: 'off' })), ['docker-lint']);
  assert.deepEqual(names(filterItems(items, { enabled: 'on' })), ['code-review', 'reviewer', 'github']);
});

test('filters combine', () => assert.deepEqual(names(filterItems(items, { collection: 'everyday', client: 'opencode', enabled: 'on' })), ['reviewer']));

test('sorting: name, updated (newest first), issues, enabled; unknown sort falls back to name', () => {
  assert.deepEqual(names(sortItems(items, 'name')), ['code-review', 'docker-lint', 'github', 'reviewer']);
  assert.deepEqual(names(sortItems(items, 'updated')), ['docker-lint', 'github', 'code-review', 'reviewer']);
  assert.deepEqual(names(sortItems(items, 'issues')).slice(0, 2).sort(), ['docker-lint', 'github']);
  assert.equal(sortItems(items, 'enabled')[0].name, 'reviewer');
  assert.deepEqual(names(sortItems(items, 'nope')), names(sortItems(items, 'name')));
});

test('facets count tags/sources/kinds; helpers', () => {
  const f = facets(items);
  assert.deepEqual(f.tags.find((t) => t.value === 'git'), { value: 'git', n: 1 });
  assert.equal(f.sources.length, 2);
  assert.equal(itemId(items[0]), 'skill/code-review');
  assert.equal(activeFilterCount({ q: 'x', kind: 'skill', tag: '' }), 1);
  assert.notEqual(sortItems(items, 'name'), items, 'never sorts in place');
});
