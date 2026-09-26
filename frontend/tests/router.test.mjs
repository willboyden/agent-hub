import test from 'node:test';
import assert from 'node:assert/strict';
import { parseHash, buildHash, compileRoute, matchRoutes, createRouter } from '../js/router.js';

test('parseHash', () => {
  assert.deepEqual(parseHash(''), { path: '/', segments: [], query: {} });
  assert.deepEqual(parseHash('#/models?tab=local&q=a%2Fb'), { path: '/models', segments: ['models'], query: { tab: 'local', q: 'a/b' } });
  assert.equal(parseHash('#instances/').path, '/instances');
  assert.equal(parseHash('#//a///b').path, '/a/b');
});

test('buildHash drops empty query values and encodes', () => {
  assert.equal(buildHash('/launch', { repo: 'org/m', x: '', y: null }), '#/launch?repo=org%2Fm');
  assert.equal(buildHash('/'), '#/');
});

test('compileRoute / matchRoutes: params, splat, precedence, decoding', () => {
  const routes = [{ path: '/' , id: 'home' }, { path: '/instances', id: 'list' }, { path: '/instances/:id', id: 'detail' }, { path: '/files/:p*', id: 'splat' }];
  assert.equal(matchRoutes(routes, '/').route.id, 'home');
  assert.equal(matchRoutes(routes, '/instances').route.id, 'list');
  const d = matchRoutes(routes, '/instances/inst%201');
  assert.equal(d.route.id, 'detail'); assert.equal(d.params.id, 'inst 1');
  assert.equal(matchRoutes(routes, '/files/a/b/c').params.p, 'a/b/c');
  assert.equal(matchRoutes(routes, '/nope'), null);
  assert.equal(matchRoutes(routes, '/instances/a/b'), null);
  assert.ok(compileRoute('/a.b').re.test('/a.b') && !compileRoute('/a.b').re.test('/aXb'));
});

test('malformed percent-encoding does not throw', () => {
  const r = matchRoutes([{ path: '/x/:id' }], '/x/%E0%A4%A');
  assert.equal(r.params.id, '%E0%A4%A');
});

test('createRouter dispatches on hashchange with a fake window', () => {
  const listeners = {}; const seen = [];
  const win = { location: { hash: '#/models?tab=local' }, addEventListener: (e, f) => { listeners[e] = f; }, removeEventListener: (e) => { delete listeners[e]; } };
  const r = createRouter({ routes: [{ path: '/models', tag: 'x' }], win, onNavigate: (n) => seen.push(n), notFound: { path: '*', tag: 'nf' } });
  r.start();
  assert.equal(seen[0].route.tag, 'x'); assert.equal(seen[0].query.tab, 'local');
  win.location.hash = '#/zzz'; listeners.hashchange();
  assert.equal(seen[1].route.tag, 'nf');
  r.go('/models', { a: 1 }); assert.equal(win.location.hash, '#/models?a=1');
  r.stop(); assert.equal(listeners.hashchange, undefined);
});
