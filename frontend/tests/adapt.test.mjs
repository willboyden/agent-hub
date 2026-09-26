import test from 'node:test';
import assert from 'node:assert/strict';
import { normalizeItem, detailDraft, toPutBody, collectionDoc, bulkOutcome, normalizeChanges, normalizeHistory, normalizeDrift, driftProblems, normalizeApply, normalizeAudit, normalizeSettings, floorRows, isFloorKey, normalizeImportResult, normalizeDiscover, clientDoc, normalizeClient, normalizeEffective, normalizeInbox, validTag } from '../js/adapt.js';

test('items: collections beat sidecar groups, updated -> updated_at, issue severity follows validity', () => {
  const n = normalizeItem({ name: 'a', groups: ['sidecar'], collections: ['c1'], updated: 5, valid: false, issues: [{ path: 'description', message: 'is empty', fix_hint: 'x' }] }, 'skill');
  assert.deepEqual([n.kind, n.groups, n.updated_at, n.issues[0].severity, n.issues[0].message], ['skill', ['c1'], 5, 'error', 'description: is empty']);
  assert.equal(normalizeItem({ name: 'a', issues: [{ message: 'm' }] }, 'agent').issues[0].severity, 'warn');
  assert.deepEqual(normalizeItem({ name: 'a', groups: ['g'] }, 'skill').groups, ['g'], 'falls back to groups when no collections field');
});

test('skill detail -> draft -> PUT body: meta wins over truncated summary, files map drops SKILL.md/hub.yaml, groups never sent', () => {
  const raw = { name: 's', description: 'truncated...', meta: { name: 's', description: 'The full description of the skill' }, body: '# b', collections: ['c'], files: { 'SKILL.md': 'x', 'hub.yaml': 'y', 'references/a.md': 'A' }, sidecar: { tags: ['t'], notes: 'n' } };
  const d = detailDraft('skill', raw);
  assert.equal(d.description, 'The full description of the skill'); assert.deepEqual(d.files, [{ path: 'references/a.md', content: 'A', binary: false }]); assert.deepEqual(d.tags, ['t']); assert.deepEqual(d.groups, ['c']);
  const body = toPutBody('skill', d); assert.deepEqual(body, { description: 'The full description of the skill', body: '# b', tags: ['t'], notes: 'n', files: { 'references/a.md': 'A' } }); assert.ok(!('groups' in body));
});

test('a skill with binary files never sends `files` (the backend would delete what is missing)', () => {
  const d = detailDraft('skill', { name: 's', meta: { description: 'x'.repeat(30) }, body: '', files: { 'SKILL.md': 'x', 'img.png': null } });
  assert.equal(d.files[0].binary, true); assert.ok(!('files' in toPutBody('skill', d)));
  assert.ok('files' in toPutBody('skill', { ...d, files: [] }));
});

test('every kind PUTs exactly its own keys (the models forbid extras)', () => {
  const agent = detailDraft('agent', { name: 'a', meta: { description: 'd', capabilities: ['read'], model_tier: 'fast', mode: 'primary', read_only: true, tags: ['x'] }, body: 'b' });
  assert.deepEqual(Object.keys(toPutBody('agent', agent)).sort(), ['body', 'capabilities', 'description', 'mode', 'model_tier', 'read_only', 'tags']);
  const ins = detailDraft('instruction', { name: 'i', meta: { title: 'T', order: 30, applies_to: ['c'] }, body: 'b' });
  assert.deepEqual(toPutBody('instruction', ins), { title: 'T', body: 'b', order: 30, applies_to: ['c'], tags: [] });
  const mcp = detailDraft('mcp', { name: 'm', meta: { transport: 'http', url: 'https://x', env_names: ['A'], scan_status: 'clean', notes: 'n' } });
  assert.equal(mcp.scan.status, 'clean'); assert.ok(!('scan_status' in toPutBody('mcp', mcp)) && !('description' in toPutBody('mcp', mcp)));
  const rule = detailDraft('rule', { name: 'r', kind: 'rule', meta: { title: 'R', kind: 'command', match: 'rm *', decision: 'deny' } });
  assert.deepEqual([rule.rule_kind, toPutBody('rule', rule).kind, toPutBody('rule', rule).reason], ['command', 'command', null]);
  assert.deepEqual(Object.keys(toPutBody('memory', detailDraft('memory', { name: 'm', meta: { type: 'user', title: 'T' }, body: 'b' }))).sort(), ['body', 'tags', 'title', 'type']);
  assert.equal(validTag('ok-tag'), true); assert.equal(validTag('Bad Tag'), false);
});

test('collectionDoc always yields a full CollectionDoc without computed fields', () => {
  const doc = collectionDoc({ id: 'c', title: 'T', member_count: 3, extra: 1, members: [{ kind: 'skill', name: 'a', junk: 1 }] }, { description: 'd' });
  assert.deepEqual(doc, { id: 'c', title: 'T', description: 'd', icon: '', color: '', order: 100, members: [{ kind: 'skill', name: 'a' }] });
});

test('bulk outcome reads both the real {results} and the sketch {changed, skipped} shapes', () => {
  assert.deepEqual(bulkOutcome({ results: [{ ok: true }, { ok: false, client: 'c', kind: 'skill', name: 'n', error: 'blocked', code: 'cell_blocked' }] }), { changed: 1, skipped: [{ client: 'c', kind: 'skill', name: 'n', reason: 'blocked' }] });
  assert.deepEqual(bulkOutcome({ changed: 2, skipped: [] }), { changed: 2, skipped: [] });
  assert.deepEqual(bulkOutcome(null), { changed: 0, skipped: [] });
});

test('changes/history normalise git statuses and the time field', () => {
  const c = normalizeChanges({ content_hash: 'h', count: 3, items: [{ path: 'a', status: 'added', diff: 'd' }, { path: 'b', status: 'renamed' }, { path: 'c', status: 'M' }] });
  assert.deepEqual(c.files.map((f) => f.status), ['added', 'modified', 'modified']); assert.equal(c.content_hash, 'h');
  assert.deepEqual(normalizeHistory([{ commit: 'c', message: 'm', author: 'a', time: 9 }]), [{ commit: 'c', message: 'm', author: 'a', ts: 9, files: null, reverts: undefined }]);
});

test('drift problems exclude in_sync and advisory; apply results are flattened and ok only when applied/nothing', () => {
  const d = normalizeDrift({ client: 'c', status: 'drift', files: [{ path: 'a', state: 'in_sync' }, { path: 'b', state: 'advisory' }, { path: 'c', state: 'changed_live' }, { path: 'd', state: 'match' }] });
  assert.deepEqual(driftProblems(d).map((f) => f.path), ['c']);
  const a = normalizeApply({ plan_id: 'p', content_hash: 'h', results: [{ client: 'x', status: 'applied', written: [{ path: 'a' }, { path: 'b' }], removed: [], skipped_conflicts: [{ path: 'z' }], backup_dir: '/b', checks: [{ name: 'n', ok: true }], verify_ok: true, blocked_reasons: [] }, { client: 'y', status: 'blocked', blocked_reasons: ['r1', 'r2'] }] });
  assert.deepEqual([a.results[0].written, a.results[0].skipped, a.results[0].backup, a.results[1].reason, a.ok], [2, ['z'], '/b', 'r1; r2', false]);
  assert.equal(normalizeApply({ results: [{ client: 'x', status: 'nothing' }] }).ok, true);
});

test('audit rows, settings and floor rows from the real shapes', () => {
  assert.deepEqual(normalizeAudit({ id: 1, ts: 5, actor: 'admin', role: 'admin', method: 'POST', path: '/api/v1/plan', status: 409, params: { keys: ['a'] } }), { id: 1, ts: 5, actor: 'admin', role: 'admin', action: 'POST /api/v1/plan', target: '/api/v1/plan', detail: '{"keys":["a"]}', ok: false, status: 409 });
  assert.deepEqual(normalizeSettings({ editable: { plan_retention: 5 }, info: { host: 'h' } }), { editable: { plan_retention: 5 }, info: { host: 'h' } });
  assert.deepEqual(normalizeSettings({ plan_retention: 5, content_dir: 'x' }).editable, { plan_retention: 5 });
  const rows = floorRows({ network_default: 'deny', deny_path_read: ['~/a/**'], deny_path_write: ['~/b'], tools: { shell: { default: 'ask', max: 'ask' } }, command_allow_forbid_patterns: ['*'], mcp: { require_scan_clean: true, require_explicit_egress: false } });
  assert.deepEqual(rows.map((r) => r.title), ['floor.network', 'floor.path_read', 'floor.path_write', 'floor.tool', 'floor.command', 'floor.mcp_scan']);
  assert.ok(rows.every((r) => isFloorKey(r.reason))); assert.equal(rows[3].extra, 'ask');
  assert.equal(floorRows({ rules: [{ title: 'T', kind: 'tool', match: 'shell', decision: 'ask', reason: 'why' }] })[0].reason, 'why');
});

test('discover/import: grouped rows, conflict words, statuses', () => {
  const d = normalizeDiscover({ client: 'c', items: { skill: [{ name: 'a', conflict: 'identical', files: ['SKILL.md'] }, { name: 'b', conflict: { existing_source: 's', equal: false, differences: ['x'] } }, { name: 'c', conflict: 'none' }], rule: [{ name: 'r', conflict: 'none', floor_violation: 'bad' }] }, notes: ['n'] });
  assert.deepEqual(d.items.map((i) => [i.kind, i.conflict === null ? null : i.conflict.equal]), [['skill', true], ['skill', false], ['skill', null], ['rule', null]]); assert.equal(d.items[3].floor_violation, 'bad'); assert.equal(d.items[0].files, 1);
  const r = normalizeImportResult({ results: [{ kind: 'skill', name: 'a', final_name: 'a', status: 'created' }, { kind: 'skill', name: 'b', final_name: 'b-2', status: 'renamed' }, { kind: 'skill', name: 'c', status: 'replaced' }, { kind: 'skill', name: 'l', status: 'linked' }, { kind: 'skill', name: 'd', status: 'identical' }, { kind: 'skill', name: 'e', status: 'skipped' }, { kind: 'skill', name: 'f', status: 'error', detail: 'x' }] });
  assert.deepEqual([r.imported.length, r.linked.length, r.renamed[0].to, r.replaced.length, r.identical.length, r.skipped.length, r.errors[0].detail], [1, 1, 'b-2', 1, 1, 1, 'x']);
});

test('client docs: PUT sends the full document with only the explicit manage flags changed', () => {
  const c = normalizeClient({ id: 'x', adapter: 'a', display_name: 'X', roots: { h: '~' }, params: { p: 1 }, strict: false, manage: { skills: false }, manage_effective: { skills: false, agents: true, mcp: false }, caps: null, status: 'in_sync' });
  assert.deepEqual([c.manage_explicit, c.manage.agents, c.strict], [{ skills: false }, true, false]);
  assert.deepEqual(clientDoc(c, { manage: { mcp: true } }), { adapter: 'a', display_name: 'X', description: '', roots: { h: '~' }, params: { p: 1 }, strict: false, manage: { skills: false, mcp: true } });
  assert.equal(normalizeClient({ adapter_installed: false }).unusable, true);
});

test('effective: items with provenance -> names, rules marked floor-derived, egress and warnings unified', () => {
  const e = normalizeEffective({ client: 'c', source: 'committed', caps: { tool_map: { read: 'Read', browser: null } }, items: { skills: [{ name: 'b', via: ['direct'] }, { name: 'a', via: ['collection:x'] }], agents: [] },
    rules: [{ kind: 'tool', match: 'shell', decision: 'ask', reason: 'policy floor: at most ask' }, { kind: 'command', match: 'rm *', decision: 'deny', reason: null }], egress: { default: 'deny', allow_hosts: ['h'] }, warnings: ['w1'], findings: [{ severity: 'error', message: 'f1' }], diagnostics: [{ severity: 'warn', message: 'w1' }] });
  assert.deepEqual(e.items.skills, ['a', 'b']); assert.deepEqual(e.rules.map((r) => r.source), ['floor', 'content']); assert.deepEqual(e.egress, { default: 'deny', allowed: ['h'] });
  assert.deepEqual(e.warnings.map((w) => `${w.severity}:${w.message}`), ['warn:w1', 'error:f1']); assert.deepEqual(e.tools, [{ capability: 'read', native: 'Read' }, { capability: 'browser', native: null }]);
  assert.equal(normalizeInbox({ id: 'i', created: 7 }).created_at, 7);
});

import { normalizeBackends } from '../js/adapt.js';
test('knowledge backends: the real object-by-name shape and the list shape both become a list', () => {
  const l = normalizeBackends({ backends: { lancedb: { ok: true, path: '/p', tables: 2 }, qdrant: { ok: false, url: 'http://q' } }, namespaces: [] });
  assert.deepEqual(l.map((x) => [x.name, x.kind, x.tables]), [['lancedb', 'embedded', 2], ['qdrant', 'service', undefined]]);
  assert.deepEqual(normalizeBackends({ items: [{ name: 'a' }] }), [{ name: 'a' }]); assert.deepEqual(normalizeBackends(null), []);
});

import { groupFindings, doctorCounts } from '../js/adapt.js';
test('doctor findings are grouped by severity; unknown severities become notes; fixes stay plain text', () => {
  const g = groupFindings([{ id: 'a', severity: 'crit', title: 'A', fix: 'x\ny' }, { id: 'b', severity: 'ok', title: 'B' }, { id: 'c', severity: 'weird', title: 'C' }, { id: 'd', severity: 'warn' }]);
  assert.deepEqual([g.crit.length, g.warn.length, g.info.length, g.ok.length], [1, 1, 1, 1]); assert.equal(g.crit[0].fix, 'x\ny'); assert.equal(g.warn[0].title, 'd');
  assert.deepEqual(groupFindings(null), { crit: [], warn: [], info: [], ok: [] }); assert.deepEqual(doctorCounts({ crit: 2 }), { crit: 2, warn: 0, info: 0 }); assert.deepEqual(doctorCounts(null), { crit: 0, warn: 0, info: 0 });
});
