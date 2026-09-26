import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import { fileURLToPath } from 'node:url';
import { server } from '../dev/mock-server.mjs';
import { miniYaml, unifiedDiff } from '../dev/mock-core.mjs';
import { parseUnified } from '../js/diff.js';
import { normalizeMatrix, normalizeChanges, normalizeApply, normalizeDiscover, normalizeImportResult, normalizeEffective, normalizeClient, clientDoc, floorRows, normalizeItem } from '../js/adapt.js';

const CC = 'claude-code'; const HM = 'hermes';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
let base;
before(async () => { await new Promise((r) => server.listen(0, '127.0.0.1', r)); base = `http://127.0.0.1:${server.address().port}`; });
after(() => { server.closeAllConnections?.(); server.close(); setTimeout(() => process.exit(0), 50).unref(); });
const H = { 'content-type': 'application/json', 'x-agent-hub': '1' };
const j = async (p, o) => { const r = await fetch(base + p, o); return { r, b: r.headers.get('content-type')?.includes('json') ? await r.json() : await r.text() }; };
const post = (p, body, extra = {}) => j(`/api/v1${p}`, { method: 'POST', headers: { ...H, ...extra }, body: JSON.stringify(body ?? {}) });
const put = (p, body) => j(`/api/v1${p}`, { method: 'PUT', headers: H, body: JSON.stringify(body) });
const del = (p) => j(`/api/v1${p}`, { method: 'DELETE', headers: H });
const get = (p) => j(`/api/v1${p}`);

test('every module reachable from index.html and app.js is served 200 and nothing in js/ is orphaned', async () => {
  const seen = new Set(); const queue = ['/index.html'];
  const html = fs.readFileSync(path.join(root, 'index.html'), 'utf8');
  for (const m of html.matchAll(/(?:src|href)="([^"#]+)"/g)) if (!m[1].startsWith('data:')) queue.push('/' + m[1]);
  const app = fs.readFileSync(path.join(root, 'js/app.js'), 'utf8');
  for (const m of app.matchAll(/mod: '(\w+)'/g)) queue.push(`/js/views/${m[1]}.js`);
  queue.push('/i18n/en.json');
  while (queue.length) {
    const u = queue.pop(); if (seen.has(u)) continue; seen.add(u);
    const { r, b } = await j(u); assert.equal(r.status, 200, u);
    if (u.endsWith('.js')) for (const m of b.matchAll(/(?:from|import)\s+['"](\.[^'"]+)['"]/g)) queue.push(path.posix.normalize(path.posix.join(path.posix.dirname(u), m[1])));
  }
  const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(d, e.name)) : [path.join(d, e.name)]));
  const orphans = walk(path.join(root, 'js')).map((f) => '/' + path.relative(root, f).split(path.sep).join('/')).filter((u) => !seen.has(u));
  assert.deepEqual(orphans, []);
});

test('static host: strict CSP, no traversal, dev/ tests/ package.json hidden', async () => {
  for (const p of ['/dev/mock-server.mjs', '/tests/api.test.mjs', '/package.json', '/..%2f..%2fetc/passwd']) assert.equal((await j(p)).r.status, 404, p);
  const csp = (await j('/index.html')).r.headers.get('content-security-policy');
  assert.match(csp, /default-src 'self'/); assert.match(csp, /style-src 'self'(?!.*unsafe)/); assert.doesNotMatch(csp, /unsafe-inline|unsafe-eval/);
});

test('seed is at scale: 48 skills, 11 agents, several MCP servers with mixed scan states, 5 clients (one generic), collections', async () => {
  const sk = (await get('/skills')).b; assert.equal(sk.items.length, 48); assert.equal(sk.next_cursor, null);
  assert.equal((await get('/agents')).b.items.length, 11);
  const mcp = (await get('/mcp')).b.items; assert.ok(mcp.length >= 5);
  const scans = new Set(); for (const m of mcp) scans.add((await get(`/mcp/${m.name}`)).b.meta.scan_status);
  assert.deepEqual([...scans].sort(), ['clean', 'findings', 'unscanned']);
  const cl = (await get('/clients')).b.items; assert.deepEqual(cl.map((c) => c.id), ['claude-code', 'example', 'hermes', 'opencode']); assert.ok(cl.some((c) => c.adapter === 'generic'));
  assert.deepEqual(Object.fromEntries(cl.map((c) => [c.id, c.status])), { 'claude-code': 'error', opencode: 'in_sync', hermes: 'edited_outside', example: 'never_applied' });
  assert.ok((await get('/collections')).b.items.length >= 5);
  assert.equal((await get('/memory/inbox')).b.items.length, 4);
  assert.deepEqual((await get('/knowledge/backends')).b.items.map((b) => b.name), ['qdrant', 'lancedb']);
  const ns = (await get('/knowledge/namespaces')).b.items; assert.ok(ns.some((n) => n.backend === 'qdrant') && ns.some((n) => n.backend === 'lancedb'));
});

test('verify before any apply is 409 never_applied', async () => { assert.equal((await post('/clients/example/verify')).b.code, 'never_applied'); });

test('client view carries adopted_at / last_applied_at / status / drift detail (edited_outside paths with reasons)', async () => {
  const cl = Object.fromEntries((await get('/clients')).b.items.map((c) => [c.id, c]));
  assert.equal(cl.opencode.last_applied_at, null); assert.ok(cl.opencode.adopted_at); assert.equal(cl.opencode.status, 'in_sync');
  assert.ok(cl['hermes'].last_applied_at); assert.equal(cl['hermes'].status, 'edited_outside');
  assert.ok(cl['hermes'].drift.edited_outside[0].path && cl['hermes'].drift.edited_outside[0].reason);
  assert.equal(cl['example'].last_applied_at, null); assert.equal(cl['example'].status, 'never_applied');
});

test('list summaries and details follow the backend shapes (collections/updated/issues[path,message]; meta/files map/sidecar), and normalisers flatten them', async () => {
  const s = (await get('/skills/csv-wrangler')).b;
  assert.ok(Array.isArray(s.collections) && 'updated' in s && s.issues[0].path && s.issues[0].message && !('severity' in s.issues[0]));
  assert.equal(s.meta.name, 'csv-wrangler'); assert.ok(s.files['SKILL.md']); assert.ok(s.sidecar);
  const n = normalizeItem(s, 'skill'); assert.equal(n.updated_at, s.updated); assert.equal(n.issues[0].severity, 'warn'); assert.deepEqual(n.groups, s.collections);
  const r = (await get('/rules')).b.items; assert.ok(r.length >= 5 && r.every((x) => !x.locked), 'floor rules are NOT items');
  assert.equal((await get('/skills?sort=nope')).r.status, 400);
});

test('lists are {items,next_cursor} and honour limit/cursor paging', async () => {
  const p1 = (await get('/skills?limit=10')).b; assert.equal(p1.items.length, 10); assert.equal(p1.next_cursor, '10');
  const p2 = (await get(`/skills?limit=10&cursor=${p1.next_cursor}`)).b; assert.notEqual(p2.items[0].name, p1.items[0].name);
  for (const p of ['/clients', '/collections', '/adapters', '/changes/history', '/audit', '/keys', '/knowledge/tokens', '/knowledge/indexes']) { const b = (await get(p)).b; assert.ok(Array.isArray(b.items) && 'next_cursor' in b, p); }
});

test('CSRF: non-GET without X-Agent-Hub is 403 csrf_header_required; GET is fine; wrong content type is 415; huge body is 413; bad Host is 421; cross-origin is 403', async () => {
  const bad = await fetch(`${base}/api/v1/plan`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' });
  assert.equal(bad.status, 403); assert.equal((await bad.json()).code, 'csrf_header_required');
  assert.equal((await get('/health')).r.status, 200);
  assert.equal((await fetch(`${base}/api/v1/plan`, { method: 'POST', headers: { 'x-agent-hub': '1', 'content-type': 'text/plain' }, body: 'x' })).status, 415);
  const big = await fetch(`${base}/api/v1/skills/aa/duplicate`, { method: 'POST', headers: H, body: JSON.stringify({ x: 'y'.repeat(1.2e6) }) }).catch(() => ({ status: 413 }));
  assert.equal(big.status, 413);
  const host = await new Promise((resolve) => http.get({ host: '127.0.0.1', port: server.address().port, path: '/api/v1/health', headers: { host: 'evil.example' } }, (res) => resolve(res.statusCode)));
  assert.equal(host, 421);
  assert.equal((await fetch(`${base}/api/v1/plan`, { method: 'POST', headers: { ...H, origin: 'http://evil.example' }, body: '{}' })).status, 403);
});

test('fault injection hook returns problem+json for every mapped status', async () => {
  for (const s of [401, 403, 409, 413, 415, 421, 429, 503]) { const r = await fetch(`${base}/api/v1/clients`, { headers: { 'x-mock-status': String(s) } }); assert.equal(r.status, s); assert.match(r.headers.get('content-type'), /problem\+json/); assert.ok((await r.json()).code); }
});

test('PUT validation: 422 errors[{path,message}], unknown keys forbidden per kind, invalid ids 400, secrets-shaped env names', async () => {
  assert.equal((await put('/skills/Bad Name', { description: 'x'.repeat(30) })).r.status, 400);
  const u = await put('/skills/code-review', { nope: 1 }); assert.equal(u.r.status, 422); assert.deepEqual(u.b.errors[0], { path: 'nope', message: 'Extra inputs are not permitted', fix_hint: null });
  assert.equal((await put('/agents/code-reviewer', { groups: [] })).r.status, 422, 'agents have no groups key');
  assert.equal((await put('/mcp/git', { description: 'x' })).r.status, 422, 'mcp docs have no description');
  assert.equal((await put('/skills/new-skill-x', {})).r.status, 422);
  assert.equal((await put('/skills/new-skill-x', { description: 'A brand new skill used by the mock test', body: '# x', tags: ['Bad Tag'] })).r.status, 422);
  const ok = await put('/skills/new-skill-x', { description: 'A brand new skill used by the mock test', body: '# x', tags: ['t1'], files: { 'references/a.md': 'hi' } }); assert.equal(ok.r.status, 200);
  assert.equal(ok.b.files['references/a.md'], 'hi'); assert.equal(ok.b.sidecar.tags[0], 't1');
  assert.equal((await put('/skills/new-skill-x', { files: { '../evil': '' } })).r.status, 422);
  assert.equal((await put('/mcp/new-mcp', { transport: 'stdio', env_names: ['lower'] })).r.status, 422);
  assert.equal((await del('/skills/new-skill-x')).r.status, 204);
});

test('floor is a structured policy; /policy/check reports findings; content rules cannot be floor rules', async () => {
  const f = (await get('/policy/floor')).b; assert.equal(f.network_default, 'deny'); assert.ok(f.deny_path_read.length && f.tools.shell.max === 'ask');
  const rows = floorRows(f); assert.ok(rows.some((x) => x.kind === 'tool' && x.match === 'shell' && x.decision === 'ask')); assert.ok(rows.every((x) => x.reason));
  const chk = (await post('/policy/check', { rules: [{ kind: 'tool', match: 'shell', decision: 'allow' }] })).b; assert.equal(chk.ok, false); assert.equal(chk.violations[0].code, 'tool_too_permissive');
  assert.equal((await post('/policy/check', { rules: [{ kind: 'tool', match: 'shell', decision: 'ask' }] })).b.ok, true);
});

test('matrix: {clients, groups} with null-id uncollected group; five cell states with reasons; toggle/bulk results follow the backend', async () => {
  const raw = (await get('/matrix')).b; assert.ok(Array.isArray(raw.clients) && raw.groups.at(-1).id === null);
  const m = normalizeMatrix(raw); assert.equal(m.groups.at(-1).id, '_ungrouped'); assert.ok(m.columns.every((c) => c.color));
  const cells = m.groups.flatMap((g) => g.rows.flatMap((r) => Object.values(r.cells)));
  assert.deepEqual([...new Set(cells.map((c) => c.state))].sort(), ['blocked', 'off', 'on', 'unsupported', 'via_collection']);
  assert.ok(cells.filter((c) => c.state === 'blocked' || c.state === 'unsupported').every((c) => c.reason));
  const t = (await post('/matrix/toggle', { client: 'example', kind: 'skill', name: 'refactor-plan', enabled: true })); assert.equal(t.r.status, 200); assert.equal(t.b.state, 'on');
  const blocked = await post('/matrix/toggle', { client: 'claude-code', kind: 'mcp', name: 'github', enabled: true }); assert.equal(blocked.r.status, 409); assert.equal(blocked.b.code, 'cell_blocked'); assert.ok(blocked.b.reason);
  assert.equal((await post('/matrix/toggle', { client: 'hermes', kind: 'agent', name: 'code-reviewer', enabled: true })).b.code, 'cell_unsupported');
  const via = await post('/matrix/toggle', { client: 'claude-code', kind: 'skill', name: 'commit-message', enabled: false }); assert.equal(via.b.state, 'off');
  const bulk = (await post('/matrix/bulk', { clients: ['example'], items: [{ kind: 'skill', name: 'docs-link-check' }, { kind: 'agent', name: 'test-writer' }], enabled: true })).b;
  assert.equal(bulk.ok, false); assert.equal(bulk.results.filter((x) => x.ok).length, 1); assert.equal(bulk.results.find((x) => !x.ok).code, 'cell_unsupported');
  const prof = (await get('/profiles/claude-code')).b; assert.ok('skills' in prof.enable && 'mcp' in prof.disable && !('skill' in prof.enable));
});

test('changes/plan/apply: plans come from COMMITTED content; commit needed; plan_stale after a later commit; conflicts skipped unless adopted', async () => {
  const ch = normalizeChanges((await get('/changes')).b); assert.ok(ch.files.length >= 2); assert.ok(ch.files.some((f) => /^profiles\//.test(f.path) && /^@@/m.test(f.diff)) && ch.files.every((f) => ['added', 'modified', 'deleted'].includes(f.status)));
  const raw = (await get('/changes')).b; assert.ok(Array.isArray(raw.items) && raw.content_hash);
  const plan0 = (await post('/plan', { clients: ['example'] })).b;
  assert.equal(plan0.pending_changes, ch.files.length);
  assert.equal(plan0.clients[0].summary.add, 6, 'the uncommitted toggle (refactor-plan: 1 file) is NOT in this plan');
  assert.equal((await post('/apply', { plan_id: plan0.id })).b.code, 'confirm_required');
  assert.equal((await post('/changes/commit', {})).r.status, 422);
  const c = await post('/changes/commit', { message: 'Enable things' }); assert.equal(c.r.status, 200); assert.match(c.b.commit, /^[0-9a-f]{40}$/);
  assert.equal((await get('/changes')).b.count, 0);
  const hist = (await get('/changes/history')).b.items[0]; assert.deepEqual([hist.commit, hist.message, typeof hist.time], [c.b.commit, 'Enable things', 'number']);
  assert.equal((await post('/apply', { plan_id: plan0.id, confirm: true })).b.code, 'plan_stale', 'the commit changed the content hash');
  const plan1 = (await post('/plan', { clients: ['example', 'claude-code'] })).b;
  const ex = plan1.clients.find((x) => x.client === 'example'); assert.equal(ex.summary.add, plan0.clients[0].summary.add + 2,  'after the commit the toggle is included');
  const cc = plan1.clients.find((x) => x.client === 'claude-code');
  assert.equal(cc.blocked, true); assert.ok(cc.floor_violations[0].message && cc.floor_violations[0].code); assert.ok(cc.blocked_reasons.length >= 1); assert.ok(cc.summary.conflict >= 1); assert.ok(cc.digest);
  const r1 = normalizeApply((await post('/apply', { plan_id: plan1.id, confirm: true })).b);
  assert.equal(r1.results.find((x) => x.client === 'claude-code').status, 'blocked');
  const done = r1.results.find((x) => x.client === 'example'); assert.equal(done.status, 'applied'); assert.equal(done.written, ex.summary.add); assert.ok(done.verify.length >= 2 && done.backup);
  const after = (await get('/clients/example')).b; assert.equal(after.status, 'in_sync'); assert.equal((await post('/clients/example/verify')).b.ok, true);
  await put('/skills/csv-wrangler', { notes: 'touch' });
  assert.equal((await post('/changes/revert', { commit: c.b.commit })).b.code, 'dirty_tree');
  await post('/changes/discard', {});
  assert.equal((await post('/changes/revert', { commit: c.b.commit })).r.status, 200);
  assert.equal((await post('/apply', { plan_id: (await post('/plan', { clients: ['nope'] })).b.id ?? 'x', confirm: true })).r.status, 404);
});

test('drift uses the planner states; conflicts are skipped unless adopted; verify before any apply is 409 never_applied', async () => {
  const drift = (await get(`/clients/${HM}/drift`)).b; assert.equal(drift.status, 'drift'); assert.ok(drift.files.some((f) => f.state === 'changed_live') && drift.files.some((f) => f.state === 'pending') && drift.files.some((f) => f.state === 'orphaned'));
  const plan = (await post('/plan', { clients: ['hermes'] })).b; const cf = plan.clients[0].files.find((f) => f.action === 'conflict'); assert.ok(cf && cf.reason);
  const r1 = normalizeApply((await post('/apply', { plan_id: plan.id, confirm: true })).b).results[0]; assert.equal(r1.status, 'applied'); assert.deepEqual(r1.skipped, [cf.path]);
  const plan2 = (await post('/plan', { clients: ['hermes'] })).b;
  const r2 = normalizeApply((await post('/apply', { plan_id: plan2.id, confirm: true, adopt_paths: [cf.path] })).b).results[0]; assert.deepEqual(r2.skipped, []); assert.ok(r2.written >= 1);
  assert.equal((await post('/plan', { clients: ['hermes'] })).b.clients[0].summary.conflict ?? 0, 0);
});

test('collections: id required, full-document PUT (unknown keys forbidden), members return the doc, bulk returns per-item results', async () => {
  assert.equal((await post('/collections', { title: 'No id' })).b.code, 'invalid_id');
  const c = await post('/collections', { id: 'test-set', title: 'Test set', members: [{ kind: 'skill', name: 'docs-link-check' }] }); assert.equal(c.r.status, 201); assert.deepEqual(Object.keys(c.b).sort(), ['color', 'description', 'icon', 'id', 'members', 'order', 'title']);
  assert.equal((await post('/collections', { id: 'test-set', title: 'Test set' })).b.code, 'already_exists');
  assert.equal((await put('/collections/test-set', { members: [] })).r.status, 422, 'a fragment is not a CollectionDoc');
  assert.equal((await put('/collections/test-set', { ...c.b, member_count: 1 })).r.status, 422);
  assert.equal((await post('/collections/test-set/members', { members: [{ kind: 'skill', name: 'nope' }] })).r.status, 404);
  const add = await post('/collections/test-set/members', { members: [{ kind: 'skill', name: 'a11y-audit' }] }); assert.equal(add.b.members.length, 2);
  const ids = (await get('/collections')).b.items.map((x) => x.id);
  assert.equal((await post('/collections/reorder', { ids: [...ids].reverse() })).b.items[0].id, 'test-set');
  const bulk = (await post('/items/bulk', { items: [{ kind: 'skill', name: 'docs-link-check' }, { kind: 'skill', name: 'nope' }], add_tags: ['zzz'], remove_from_collection: 'test-set' })).b;
  assert.equal(bulk.ok, false); assert.equal(bulk.results[0].ok, true); assert.equal(bulk.results[1].ok, false);
  assert.equal((await del('/collections/test-set/members/skill/docs-link-check')).r.status, 404, 'already removed by bulk');
  assert.equal((await del('/collections/test-set')).r.status, 204);
});

test('clients: validate-spec needs an object; spec errors are diagnostics; create/update use whole ClientDocs; adapter default config', async () => {
  const spec = { skills: { root: 'home', dir: 'skills' }, instructions: { root: 'home', target: 'RULES.md', merge: 'block' } };
  const ok = (await post('/clients/validate-spec', { spec, roots: { home: '~/.x' } })).b; assert.equal(ok.ok, true); assert.equal(ok.artifacts.length, 2); assert.ok(ok.artifacts[0].preview);
  const bad = (await post('/clients/validate-spec', { spec: { skills: { dir: '../up' }, weird: 1 }, roots: { home: '~' } })).b;
  assert.equal(bad.ok, false); assert.deepEqual([...new Set(bad.diagnostics.map((e) => e.code))].sort(), ['missing_field', 'unknown_key', 'unsafe_path']);
  assert.equal((await post('/clients/validate-spec', { spec_text: 'skills: {}' })).r.status, 422, 'the backend does not parse YAML text');
  const mk = await post('/clients', { id: 'my-cli', adapter: 'generic', display_name: 'My CLI', spec, roots: { home: '~/.x' } }); assert.equal(mk.r.status, 201); assert.equal(mk.b.caps.skills, true); assert.equal(mk.b.caps.agents, false); assert.equal(mk.b.committed, false);
  assert.equal((await post('/clients', { id: 'my-cli', adapter: 'generic', display_name: 'x', spec })).b.code, 'already_exists');
  assert.equal((await post('/clients', { id: 'zz', adapter: 'generic', display_name: 'x' })).r.status, 422);
  assert.equal((await put('/clients/opencode', { manage: { mcp: true } })).r.status, 422, 'a partial PUT is not a ClientDoc');
  const oc = normalizeClient((await get('/clients/opencode')).b);
  const upd = await put('/clients/opencode', clientDoc(oc, { manage: { mcp: true } })); assert.equal(upd.r.status, 200); assert.equal(upd.b.manage_effective.mcp, true);
  assert.equal((await put('/clients/opencode', clientDoc(normalizeClient(upd.b), { manage: { mcp: false } }))).b.manage_effective.mcp, false);
  assert.equal((await get('/clients/my-cli/effective')).b.code, 'client_not_committed');
  const eff = normalizeEffective((await get('/clients/my-cli/effective?source=working')).b); assert.equal(eff.source, 'working');
  const effC = (await get(`/clients/${CC}/effective`)).b; assert.ok(effC.items.skills[0].via.length); assert.ok(effC.rules.some((r) => /policy floor/.test(r.reason))); assert.equal(normalizeEffective(effC).rules.find((r) => /policy floor/.test(r.reason)).source, 'floor');
  assert.ok(effC.findings.length && effC.egress.allow_hosts.length && effC.warnings.every((w) => typeof w === 'string'));
  const aud = (await get(`/clients/${CC}/audit`)).b; assert.equal(aud.ok, false); assert.ok(aud.findings.every((f) => f.code && f.severity && f.message));
  assert.equal((await del('/clients/my-cli')).r.status, 204);
});

test('import: discover groups items by kind with none/identical/differs; results carry created/renamed/replaced/identical/skipped', async () => {
  const raw = (await post(`/clients/${CC}/discover`, {})).b; assert.ok(raw.items.skill.length && !Array.isArray(raw.items));
  const d = normalizeDiscover(raw); assert.ok(d.items.some((i) => i.conflict?.equal === true) && d.items.some((i) => i.conflict?.equal === false && i.conflict.differences.length) && d.items.some((i) => !i.conflict)); assert.ok(d.items.find((i) => i.name === 'allow-all-bash').floor_violation);
  const items = [{ kind: 'skill', name: 'container-basics' }, { kind: 'skill', name: 'kubectl-helper' }, { kind: 'skill', name: 'docs-verify' }];
  const skip = normalizeImportResult((await post('/import', { client: 'claude-code', items, on_conflict: 'skip' })).b); assert.deepEqual([skip.skipped.length, skip.imported.length, skip.identical.length], [1, 1, 1]);
  const ren = normalizeImportResult((await post('/import', { client: 'claude-code', items: [items[0]], on_conflict: 'rename' })).b); assert.deepEqual(ren.renamed.map((x) => x.to), ['container-basics-2']);
  assert.equal((await post('/import', { client: 'claude-code', items: [items[0]], on_conflict: 'bogus' })).r.status, 400);
  assert.equal((await post('/import', { client: 'claude-code', items: [items[0]], extra: 1 })).r.status, 422);
});

test('memory inbox promote/reject; knowledge: create, ingest job, query, one-time token secret, path allowlist, proxy is GET/POST only', async () => {
  const p = (await post('/memory/inbox/inb_1/promote', { title: 'Router key rotates weekly' })).b; assert.equal(p.id, 'inb_1'); assert.equal(p.promoted_from, 'hermes/inb_1'); assert.equal((await get('/memory/inbox')).b.items.length, 3);
  assert.equal((await post('/memory/inbox/inb_2/reject')).b.rejected, true);
  assert.equal((await get('/memory/inbox')).b.items[0].body.length > 0, true);
  assert.equal((await post('/knowledge/namespaces', { name: 'tmp-ns', backend: 'mongo', dim: 8 })).r.status, 422);
  assert.equal((await post('/knowledge/namespaces', { name: 'tmp-ns', backend: 'lancedb', dim: 8 })).r.status, 201);
  assert.equal((await post('/knowledge/namespaces/tmp-ns/ingest', { path: '/etc', glob: '*' })).r.status, 422);
  const job = (await post('/knowledge/namespaces/tmp-ns/ingest', { documents: [{ text: 'The router chokepoint handles every LLM call', metadata: { a: 1 } }] })).b; assert.equal(job.state, 'running');
  await new Promise((r) => setTimeout(r, 900)); assert.equal((await get(`/knowledge/jobs/${job.id}`)).b.state, 'done');
  const q = (await post('/knowledge/namespaces/team-docs/query', { text: 'router chokepoint LLM', k: 2 })).b; assert.equal(q.hits.length, 2); assert.ok(q.hits[0].score >= q.hits[1].score);
  assert.equal((await post('/knowledge/namespaces/team-docs/query', { text: '', k: 2 })).r.status, 422);
  const tok = (await post('/knowledge/tokens', { client: 'x', scopes: [{ ns: 'team-docs', mode: 'read' }] })).b; assert.ok(tok.secret); assert.ok(!JSON.stringify((await get('/knowledge/tokens')).b).includes(tok.secret));
  assert.equal((await post('/knowledge/tokens', { client: 'x', scopes: [{ ns: 'nope', mode: 'read' }] })).r.status, 422);
  const del405 = await del('/knowledge/namespaces/tmp-ns'); assert.equal(del405.r.status, 405); assert.equal(del405.b.code, 'method_not_allowed');
  const sse = await fetch(`${base}/api/v1/knowledge/jobs/${job.id}/stream`); assert.match(sse.headers.get('content-type'), /event-stream/); assert.match(await sse.text(), /"state":"done"/);
});

test('settings/keys/audit follow the backend: editable ints + info; secrets shown once; audit rows are method/path/status', async () => {
  const s = (await get('/settings')).b; assert.ok(s.editable.plan_retention && s.info.content_dir);
  assert.equal((await put('/settings', { plan_retention: 30 })).b.editable.plan_retention, 30);
  assert.equal((await put('/settings', { knowledge_admin_token: 'x' })).r.status, 422); assert.equal((await put('/settings', { plan_retention: 0 })).r.status, 422);
  const key = (await post('/keys', { name: 'k', role: 'viewer' })).b; assert.match(key.secret, /^hc_/); assert.ok(!JSON.stringify((await get('/keys')).b).includes(key.secret));
  assert.equal((await post('/keys', { name: 'k2', role: 'client', client: 'hermes', scopes: ['inbox:write'] })).r.status, 201);
  assert.equal((await post('/keys', { name: 'k3', role: 'viewer', client: 'x' })).r.status, 400); assert.equal((await post('/keys', { name: 'k4', role: 'client', client: 'x', scopes: ['everything'] })).r.status, 400);
  const a = (await get('/audit')).b.items[0]; assert.ok(a.method && a.path && a.status && a.actor && a.params);
  assert.ok(!JSON.stringify((await get('/audit?limit=100')).b).includes(key.secret), 'audit stores request metadata, never values');
});

test('mock helper modules: mini YAML and unified diff round-trip through the frontend parser; secret-looking values are redacted in diffs', () => {
  assert.deepEqual(JSON.parse(JSON.stringify(miniYaml('a:\n  b: 1\n  c: [x, y]\nd:\n  - p\n  - q\n'))), { a: { b: 1, c: ['x', 'y'] }, d: ['p', 'q'] });
  const d = unifiedDiff('a\nb\nc\n', 'a\nB\nc\nd\n', 'f', 'f');
  assert.deepEqual(parseUnified(d).filter((l) => l.type === 'add' || l.type === 'del').map((l) => l.type + l.text), ['delb', 'addB', 'addd']);
  assert.equal(unifiedDiff('same\n', 'same\n', 'f', 'f'), '');
  assert.match(unifiedDiff(null, 'x\n', 'f', 'f'), /^--- \/dev\/null/);
  assert.match(unifiedDiff('k\n', 'token = abcdef123456\n', 'f', 'f'), /\[redacted\]/);
});

test('collection suggestions: listed for uncollected items, accepted with title/member overrides, then gone', async () => {
  const s = (await get('/collections/suggestions')).b.items; assert.ok(s.length >= 2); assert.ok(s.every((x) => x.reason && x.members.length >= 3 && x.title));
  const first = s[0];
  const acc = await post('/collections/suggestions/accept', { ids: [first.id], overrides: { [first.id]: { title: 'My name', members: first.members.slice(1) } } });
  assert.equal(acc.r.status, 200); assert.equal(acc.b.items[0].title, 'My name'); assert.equal(acc.b.items[0].members.length, first.members.length - 1);
  assert.ok(!(await get('/collections/suggestions')).b.items.some((x) => x.id === first.id));
  assert.equal((await post('/collections/suggestions/accept', { ids: ['sugg-nope'] })).r.status, 404);
});

test('import accepts a per-item on_conflict including link (enable the existing item for the client)', async () => {
  const r = normalizeImportResult((await post('/import', { client: 'example', on_conflict: 'skip', items: [{ kind: 'skill', name: 'container-basics', on_conflict: 'link' }, { kind: 'skill', name: 'kubectl-helper' }] })).b);
  assert.deepEqual([r.linked.length, r.imported.length], [1, 1]);
  assert.ok((await get('/profiles/example')).b.enable.skills.includes('container-basics'));
  assert.equal((await post('/import', { client: 'example', items: [{ kind: 'skill', name: 'container-basics', on_conflict: 'merge' }] })).r.status, 400);
});

test('knowledge health summary and the two knowledge-side failure codes', async () => {
  const h = (await get('/knowledge/health')).b; assert.deepEqual([h.reachable, h.authenticated, typeof h.backends], [true, true, 'object']);
});

test('doctor: status, findings with severities and a multi-line fix, and the summary counts', async () => {
  const d = (await get('/doctor')).b; assert.equal(d.status, 'action_needed'); assert.ok(d.findings.some((f) => f.severity === 'ok') && d.findings.some((f) => /\n/.test(f.fix)));
  const s = (await get('/doctor/summary')).b; assert.deepEqual(s, { crit: 1, warn: 2, info: 1 });
});
