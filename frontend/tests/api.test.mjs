import test from 'node:test';
import assert from 'node:assert/strict';
import { createClient, ApiError, buildQuery, CSRF_HEADER, KIND_PATH } from '../js/api.js';

const resp = (status, body, ct = 'application/json') => ({ status, ok: status < 400, headers: { get: () => ct }, json: async () => body, text: async () => String(body) });

test('CSRF header is X-Agent-Hub', () => assert.equal(CSRF_HEADER, 'X-Agent-Hub'));
test('buildQuery skips empty values', () => assert.equal(buildQuery({ a: 1, b: '', c: null, d: 'x y' }), '?a=1&d=x+y'));

test('X-Agent-Hub: 1 is sent on every non-GET request and never on GET', async () => {
  const seen = [];
  const c = createClient({ fetchImpl: async (u, o) => { seen.push([o.method, o.headers[CSRF_HEADER]]); return resp(200, {}); } });
  await c.get('/x'); await c.post('/x', {}); await c.put('/x', {}); await c.del('/x');
  await c.commit('m'); await c.apply('p'); await c.matrixBulk({}); await c.kQuery('ns', { text: 'a' }); await c.reject('i');
  assert.deepEqual(seen.slice(0, 4), [['GET', undefined], ['POST', '1'], ['PUT', '1'], ['DELETE', '1']]);
  assert.ok(seen.slice(4).every(([m, h]) => m === 'POST' && h === '1'));
});

test('bearer key and JSON body', async () => {
  let call; const c = createClient({ getKey: () => 'k1', fetchImpl: async (u, o) => { call = { u, o }; return resp(200, {}); } });
  await c.post('/plan', { clients: ['a'] });
  assert.equal(call.o.headers.Authorization, 'Bearer k1'); assert.equal(call.o.body, '{"clients":["a"]}'); assert.equal(call.o.headers['Content-Type'], 'application/json');
});

test('problem+json errors become ApiError with code; 401 fires onUnauthorized; network failure is status 0', async () => {
  let fired = 0;
  const c = createClient({ onUnauthorized: () => fired++, fetchImpl: async () => resp(401, { title: 'Unauthorized', detail: 'no', code: 'unauthorized' }) });
  await assert.rejects(c.get('/x'), (e) => e instanceof ApiError && e.status === 401 && e.code === 'unauthorized'); assert.equal(fired, 1);
  const n = createClient({ fetchImpl: async () => { throw new TypeError('x'); } });
  await assert.rejects(n.get('/x'), (e) => e.status === 0 && e.code === 'network');
  const stale = createClient({ fetchImpl: async () => resp(409, { code: 'plan_stale', detail: 'stale' }) });
  await assert.rejects(stale.apply('p'), (e) => e.status === 409 && e.code === 'plan_stale');
});

test('listAll follows next_cursor and passes bare arrays through', async () => {
  const urls = [];
  const c = createClient({ fetchImpl: async (u) => { urls.push(u); return u.includes('cursor=2') ? resp(200, { items: [3], next_cursor: null }) : resp(200, { items: [1, 2], next_cursor: '2' }); } });
  assert.deepEqual(await c.listAll('/things'), [1, 2, 3]); assert.equal(urls.length, 2);
  assert.deepEqual(await createClient({ fetchImpl: async () => resp(200, [9]) }).listAll('/x'), [9]);
});

test('endpoint paths follow the contract (kinds, encoding, knowledge proxy)', async () => {
  const seen = []; const c = createClient({ fetchImpl: async (u, o) => { seen.push(`${o.method} ${u}`); return resp(200, { items: [] }); } });
  await c.saveItem('mcp', 'my server', {}); await c.item('memory', 'a'); await c.removeMember('c1', 'skill', 'a b'); await c.kNamespaces(); await c.kIngest('ns', {}); await c.discoverClient('claude-code'); await c.validateSpec({}); await c.revert('abc');
  assert.deepEqual(seen, ['PUT /api/v1/mcp/my%20server', 'GET /api/v1/memory/a', 'DELETE /api/v1/collections/c1/members/skill/a%20b', 'GET /api/v1/knowledge/namespaces?limit=200', 'POST /api/v1/knowledge/namespaces/ns/ingest', `POST /api/v1/clients/${['claude', 'code'].join('-')}/discover`, 'POST /api/v1/clients/validate-spec', 'POST /api/v1/changes/revert']);
  assert.equal(KIND_PATH.rule, 'rules');
});

test('204 resolves null', async () => assert.equal(await createClient({ fetchImpl: async () => ({ status: 204, ok: true, headers: { get: () => '' } }) }).deleteItem('skill', 'a'), null));

test('only a 401 from the hub itself opens the key dialog; knowledge-side failures never do', async () => {
  let fired = 0; const mk = (status, code) => createClient({ onUnauthorized: () => fired++, fetchImpl: async () => resp(status, { code, detail: 'x' }) });
  await assert.rejects(mk(401, 'unauthorized').get('/x'), (e) => e.isHubAuth === true && !e.isKnowledgeSetup); assert.equal(fired, 1);
  await assert.rejects(mk(401, 'knowledge_upstream_unauthorized').get('/knowledge/namespaces'), (e) => e.isHubAuth === false && e.isKnowledgeSetup);
  await assert.rejects(mk(502, 'knowledge_upstream_unauthorized').get('/knowledge/namespaces'), (e) => e.isHubAuth === false && e.isKnowledgeSetup);
  await assert.rejects(mk(503, 'knowledge_unavailable').get('/knowledge/health'), (e) => e.isHubAuth === false && e.isKnowledgeSetup);
  assert.equal(fired, 1, 'the knowledge codes did not fire onUnauthorized');
});
