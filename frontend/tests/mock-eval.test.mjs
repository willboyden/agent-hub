import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { server } from '../dev/mock-server.mjs';

let base;
before(async () => { await new Promise((r) => server.listen(0, '127.0.0.1', r)); base = `http://127.0.0.1:${server.address().port}/api/v1`; });
after(() => { server.closeAllConnections?.(); server.close(); setTimeout(() => process.exit(0), 50).unref(); });
const H = { 'content-type': 'application/json', 'x-agent-hub': '1' };
const j = async (p, o) => { const r = await fetch(base + p, o); return { r, b: await r.json().catch(() => null) }; };
const post = (p, body) => j(p, { method: 'POST', headers: H, body: JSON.stringify(body ?? {}) });
const put = (p, body) => j(p, { method: 'PUT', headers: H, body: JSON.stringify(body) });

test('evaluator status lists only the three tiers', async () => {
  const { b } = await j('/skill-evaluator');
  assert.deepEqual(b.tiers.map((t) => t.id), ['static', 'tier1', 'tier1+2']);
});

test('list: hub rows keep `skill`, observed rows carry hub_status, latest omits heavy validator fields, gate is a string', async () => {
  const hub = (await j('/skill-evaluations')).b;
  assert.ok(hub.items.length >= 10 && hub.items.every((x) => x.source === 'hub' && x.skill === x.name));
  const ev = hub.items.find((x) => x.latest);
  assert.ok(!('validators' in ev.latest) && !('security_findings' in ev.latest)); assert.equal(typeof ev.gate, 'string');
  const her = (await j('/skill-evaluations?source=hermes')).b;
  assert.ok(her.items.length >= 40); assert.ok(her.items.every((x) => x.hub_status));
  assert.equal(her.summary.total, her.items.length);
  assert.ok(new Set(her.items.map((x) => x.origin)).size >= 5);
  const cc = (await j('/skill-evaluations?source=claude-code')).b; assert.equal(cc.items.length, 8);
  assert.equal((await j('/skill-evaluations?source=nope')).r.status, 400);
  assert.ok(her.items.some((x) => x.gate === 'blocked') && her.items.some((x) => x.gate === 'warn') && her.items.some((x) => x.gate === 'missing') && her.items.some((x) => x.gate === 'ok'));
});

test('detail has both passes and the gate object; the report is Markdown', async () => {
  const d = (await j('/skill-evaluations/' + encodeURIComponent('hub:docs-verify'))).b;
  assert.ok(Array.isArray(d.latest.validators) && Array.isArray(d.latest.validators_llm)); assert.equal(d.gate.status, 'ok'); assert.ok(Array.isArray(d.gate.reasons));
  const rep = (await j(`/skill-evaluations/${encodeURIComponent('hub:docs-verify')}/runs/${d.latest.run_id}/report`)).b;
  assert.match(rep.markdown, /^# Evaluation report/);
  assert.equal((await j('/skill-evaluations/bad%20subject')).r.status, 400);
});

test('run: subjects xor select, queue advances only when polled, skipped reasons, cancel', async () => {
  assert.equal((await post('/skill-evaluations/run', { tiers: 'static' })).r.status, 422);
  assert.equal((await post('/skill-evaluations/run', { subjects: ['hub:a'], select: 'all' })).r.status, 422);
  assert.equal((await post('/skill-evaluations/run', { subjects: ['no colon'] })).r.status, 400);
  const r = await post('/skill-evaluations/run', { subjects: ['hub:a11y-audit', 'hub:nothing-here'], tiers: 'tier1', force: true });
  assert.equal(r.r.status, 202); assert.deepEqual(r.b.queued, ['hub:a11y-audit']); assert.deepEqual(r.b.skipped, [{ subject: 'hub:nothing-here', reason: 'not_found' }]);
  const again = await post('/skill-evaluations/run', { subjects: ['hub:a11y-audit'], tiers: 'static' });
  assert.equal(again.b.skipped[0].reason, 'running');
  const q1 = (await j('/skill-evaluations/queue')).b; assert.equal(q1.running, null); assert.equal(q1.finished_since_start >= 1, true);
  const big = await post('/skill-evaluations/run', { select: 'none_or_stale', source: 'claude-code', tiers: 'static' });
  assert.ok(big.b.queued.length >= 1);
  const q2 = (await j('/skill-evaluations/queue')).b; assert.ok(q2.running || q2.queued_total >= 0);
  await post('/skill-evaluations/queue/cancel'); const q3 = (await j('/skill-evaluations/queue')).b; assert.equal(q3.queued_total, 0);
});

test('observe + import: gate require refuses missing and blocked, override lets blocked through as `overridden`, enable defaults off', async () => {
  const obs = (await post('/clients/hermes/observe')).b; assert.ok(obs.total >= 40 && obs.by_origin.shipped && obs.by_hub_status.in_hub_same);
  assert.equal((await put('/settings', { eval_gate: 'bogus' })).r.status, 422);
  assert.equal((await put('/settings', { eval_gate_observed: 'require', eval_gate_min_tiers: 'static' })).r.status, 200);
  const list = (await j('/clients/hermes/observed')).b;
  const key = (n) => list.items.find((i) => i.name === n).key;
  const items = [{ key: key('curl-pipe-bash-installer') }, { key: key('arxiv-search') }, { key: key('blogwatcher') }];
  let res = (await post('/clients/hermes/observed/import', { items })).b.results;
  assert.deepEqual(res.map((x) => [x.status, x.code || null]), [['refused', 'eval_blocked'], ['created', null], ['refused', 'eval_required']]);
  assert.equal((await post('/clients/hermes/observed/import', { items: [items[0]], override_reason: 'short' })).r.status, 422);
  res = (await post('/clients/hermes/observed/import', { items: [items[0]], override_reason: 'Reviewed by hand, the script is pinned.' })).b.results;
  assert.deepEqual([res[0].status, res[0].gate], ['created', 'overridden']);
  res = (await post('/clients/hermes/observed/import', { items: [items[1]], on_conflict: 'skip' })).b.results; assert.equal(res[0].status, 'skipped');
  await put('/settings', { eval_gate_observed: 'require' });
  assert.equal((await put('/settings', { eval_gate_observed: 'nope' })).r.status, 422);
  assert.equal((await j('/settings')).b.editable.eval_gate, 'advisory');
});

test('clients carry observed_summary; drift carries observed; unobserved client answers 404 not_observed', async () => {
  const cl = (await j('/clients')).b.items; assert.equal(cl.find((c) => c.id === 'example').observed_summary, null);
  assert.ok(cl.find((c) => c.id === 'hermes').observed_summary.total >= 40);
  assert.equal((await j('/clients/example/observed')).b.code, 'not_observed');
});
