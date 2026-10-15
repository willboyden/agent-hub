import test from 'node:test';
import assert from 'node:assert/strict';
import * as E from '../js/evalview.js';
import { evalIndex } from '../js/skilleval.js';

const row = (o = {}) => ({ subject: 'hub:a', source: 'hub', name: 'a', path: 'skills/a', origin: 'hub', state: 'current', gate: 'ok', latest: { verdict: 'passed', finished_at: 100, tier1: { quality_score: 83.64, grade: 'B', judge_score: 70, severity: { critical: 0, high: 2 } } }, ...o });
const rows = [
  row(),
  row({ subject: 'h:k1', source: 'h', name: 'curl-installer', path: 'curl-installer', origin: 'agent', gate: 'blocked', latest: { verdict: 'failed', finished_at: 50, tier1: { quality_score: 40, severity: { critical: 1, high: 0 } } } }),
  row({ subject: 'h:k2', source: 'h', name: 'notes', path: 'notes', origin: 'shipped', state: 'none', gate: 'missing', latest: null }),
  row({ subject: 'h:k3', source: 'h', name: 'old', path: '.archive/old', origin: 'archived', state: 'stale', gate: 'warn', latest: { verdict: 'incomplete', finished_at: 10 } }),
  row({ subject: 'h:k4', source: 'h', name: 'run', origin: 'agent', state: 'none', gate: { status: 'ok' }, latest: null, queued: true }),
];

test('filters: each field narrows, text must match every word, source is not filtered client-side', () => {
  const names = (f) => E.filterEvalRows(rows, f).map((r) => r.name);
  assert.equal(names({}).length, 5);
  assert.deepEqual(names({ origin: 'agent' }), ['curl-installer', 'run']);
  assert.deepEqual(names({ state: 'none' }), ['notes', 'run']);
  assert.deepEqual(names({ verdict: 'failed' }), ['curl-installer']);
  assert.deepEqual(names({ gate: 'ok' }), ['a', 'run']);            // string and {status} gates both match
  assert.deepEqual(names({ q: 'ARCHIVE old' }), ['old']);
  assert.deepEqual(names({ q: 'curl zzz' }), []);
  assert.deepEqual(names({ origin: 'agent', gate: 'blocked', verdict: 'failed' }), ['curl-installer']);
  assert.equal(E.activeEvFilterCount({ source: 'x', origin: 'agent', q: 'a' }), 2);
});

test('filters round-trip through the URL query; defaults are omitted', () => {
  assert.deepEqual(E.filtersFromQuery({}), E.EMPTY_EV_FILTERS);
  assert.deepEqual(E.filtersFromQuery({ source: 'hermes', gate: 'blocked', junk: 'x' }), { ...E.EMPTY_EV_FILTERS, source: 'hermes', gate: 'blocked' });
  assert.deepEqual(E.filtersToQuery({ ...E.EMPTY_EV_FILTERS, source: 'hub', origin: 'agent' }), { origin: 'agent' });
  assert.deepEqual(E.filtersToQuery({ ...E.EMPTY_EV_FILTERS, source: 'all' }), { source: 'all' });
});

test('summary counts by state, verdict, gate; chips skip zero counts and keep a fixed order', () => {
  const s = E.summarizeRows(rows);
  assert.equal(s.total, 5);
  assert.deepEqual(s.by_state, { current: 2, none: 2, stale: 1 });
  assert.deepEqual(s.by_verdict, { passed: 1, failed: 1, incomplete: 1 });
  assert.deepEqual(s.by_gate, { ok: 2, blocked: 1, missing: 1, warn: 1 });
  assert.equal(s.queued, 1);
  const chips = E.summaryChips(s);
  assert.deepEqual(chips.map((c) => `${c.filter}:${c.value}`), ['gate:ok', 'gate:warn', 'gate:blocked', 'gate:missing', 'verdict:passed', 'verdict:failed', 'verdict:incomplete', 'state:none', 'state:stale']);
  assert.ok(!E.summaryChips(E.summarizeRows([])).length);
});

test('gate badge mapping: every status has tone, icon and text keys; off means no badge', () => {
  assert.equal(E.gateModel(null), null);
  assert.equal(E.gateModel(undefined), null);
  for (const [g, tone] of [['ok', 'ok'], ['warn', 'warn'], ['blocked', 'bad'], ['missing', 'muted'], ['overridden', 'warn']]) {
    const m = E.gateModel(g); assert.equal(m.tone, tone); assert.equal(m.key, `ev.gate_${g}`); assert.ok(m.icon);
    assert.equal(E.gateModel({ status: g }).status, g);
  }
  assert.equal(E.gateModel('weird').tone, 'muted');
});

test('verdict cell: running and queued beat the stored verdict; stale is flagged; none says not evaluated', () => {
  assert.equal(E.verdictCell(row({ running: true })).kind, 'running');
  assert.equal(E.verdictCell(row({ queued: true })).kind, 'queued');
  assert.equal(E.verdictCell(rows[2]).kind, 'none');
  const stale = E.verdictCell(rows[3]);
  assert.equal(stale.kind, 'incomplete'); assert.equal(stale.stale, true); assert.equal(stale.tone, 'muted');
  assert.equal(E.verdictCell(rows[1]).tone, 'bad');
  assert.equal(E.verdictCell(row({ latest: { verdict: 'nonsense' } })).kind, 'error');
});

test('row numbers: rounded, never invented', () => {
  const n = E.rowNumbers(rows[0]);
  assert.deepEqual([n.quality, n.judge, n.critical, n.high, n.at], ['B 83.6', '70', null, null, 100]);   // tier1.severity counts ALL validators: never shown as security
  const none = E.rowNumbers(rows[2]);
  assert.deepEqual([none.quality, none.judge, none.critical, none.high, none.at], [null, null, null, null, null]);
});

test('sorting: gate and verdict worst first, quality lowest first, evaluated newest first, stable by name', () => {
  assert.deepEqual(E.sortEvalRows(rows, 'gate').map((r) => r.name).slice(0, 3), ['curl-installer', 'old', 'notes']);
  assert.equal(E.sortEvalRows(rows, 'verdict')[0].name, 'curl-installer');
  assert.deepEqual(E.sortEvalRows(rows, 'quality').map((r) => r.name), ['curl-installer', 'a', 'notes', 'old', 'run']);
  assert.equal(E.sortEvalRows(rows, 'evaluated')[0].name, 'a');
  assert.deepEqual(E.sortEvalRows(rows, 'name').map((r) => r.name), ['a', 'curl-installer', 'notes', 'old', 'run']);
  assert.equal(rows[0].name, 'a');   // input untouched
});

test('subjects and observed rows', () => {
  assert.deepEqual(E.splitSubject('hermes:code-review-1a2b3c4d'), { source: 'hermes', key: 'code-review-1a2b3c4d' });
  assert.deepEqual(E.splitSubject('hub:x:y'), { source: 'hub', key: 'x:y' });
  assert.equal(E.isObserved(rows[0]), false); assert.equal(E.isObserved(rows[1]), true);
  assert.deepEqual(E.clientsOf(rows), ['h']);
  assert.deepEqual(E.clientsOf([rows[0]]), []);
});

test('tiers: Tier 3 never offered; unknown or empty status falls back to the three', () => {
  assert.deepEqual(E.offeredTiers(null), ['static', 'tier1', 'tier1+2']);
  assert.deepEqual(E.offeredTiers({ tiers: [{ id: 'static' }, { id: 'tier3' }, 'tier1+2'] }), ['static', 'tier1+2']);
  assert.ok(E.tierRank('tier1+2') > E.tierRank('tier1') && E.tierRank('tier1') > E.tierRank('static'));
});

test('validator passes: static and LLM labelled separately, publication validators flagged, one pass gets the plain label', () => {
  const v = (validator, status = 'passed', severity = {}) => ({ validator, status, severity });
  const both = E.validatorPasses({ validators: [v('Prompt Injection', 'failed', { high: 1 }), v('QUALITY')], validators_llm: [v('Prompt Injection', 'passed')] });
  assert.deepEqual(both.map((p) => p.pass), ['static', 'llm']);
  assert.deepEqual(both[0].rows.map((r) => r.security), [true, false]);
  assert.equal(both[0].rows[0].sev.high, 1); assert.equal(both[0].rows[0].sev.critical, 0);
  assert.equal(E.passLabelKey('static', true), 'ev.pass_static'); assert.equal(E.passLabelKey('llm', true), 'ev.pass_llm'); assert.equal(E.passLabelKey('static', false), 'ev.validators');
  assert.deepEqual(E.validatorPasses({ validators: [v('X')], validators_llm: null }).map((p) => p.pass), ['static']);
  assert.deepEqual(E.validatorPasses({ validators: [v('X')], validators_llm: [v('Y')] }).map((p) => p.pass), ['static', 'llm']);   // `validators` is the static pass
  assert.deepEqual(E.validatorPasses(null), []);
  assert.equal(E.validatorPasses({ validators: [v('Brand New Validator')] })[0].rows[0].security, true);   // fail closed
  assert.equal(E.validatorPasses({ validators: [v('X', 'weird')] })[0].rows[0].status, 'error');
});

test('queue poll state machine: polls only while non-empty, refreshes once when it drains, backs off then gives up on errors', () => {
  const active = { running: { subject: 'hub:a' }, queued: [], queued_total: 3, finished_since_start: 0 };
  const empty = { running: null, queued: [], queued_total: 0, finished_since_start: 4 };
  assert.equal(E.queueActive(active), true); assert.equal(E.queueActive(empty), false); assert.equal(E.queueActive(null), false);
  assert.equal(E.queueActive({ running: { subject: 'x' }, queued_total: 0 }), true);
  // view opened with an empty queue: stay idle, never poll
  let r = E.pollStep(E.pollInit(), { type: 'queue', queue: empty });
  assert.equal(r.delay, null); assert.equal(r.state.phase, 'idle'); assert.equal(r.refresh, false);
  // view opened with work already queued: start polling every 2 s
  r = E.pollStep(E.pollInit(), { type: 'queue', queue: active });
  assert.equal(r.delay, 2000); assert.equal(r.state.phase, 'polling');
  // progress refreshes the table; no progress does not
  let s = r.state;
  r = E.pollStep(s, { type: 'queue', queue: { ...active, finished_since_start: 1 } }); assert.equal(r.refresh, true); assert.equal(r.delay, 2000); s = r.state;
  r = E.pollStep(s, { type: 'queue', queue: { ...active, finished_since_start: 1 } }); assert.equal(r.refresh, false);
  // drained: stop polling and refresh once
  r = E.pollStep(r.state, { type: 'queue', queue: empty }); assert.equal(r.delay, null); assert.equal(r.refresh, true); assert.equal(r.state.phase, 'idle');
  // after that an empty queue refreshes nothing
  assert.equal(E.pollStep(r.state, { type: 'queue', queue: empty }).refresh, false);
  // enqueueing starts polling soon
  r = E.pollStep(E.pollInit(), { type: 'start' }); assert.equal(r.state.phase, 'polling'); assert.ok(r.delay > 0 && r.delay < 2000);
  // errors back off, then stop
  let e = E.pollStep(r.state, { type: 'error' }); assert.equal(e.delay, 2000);
  e = E.pollStep(e.state, { type: 'error' }); assert.equal(e.delay, 4000);
  e = E.pollStep(e.state, { type: 'error' }); assert.equal(e.delay, null); assert.equal(e.state.phase, 'failed');
  // leaving the view
  assert.equal(E.pollStep(s, { type: 'stop' }).delay, null);
  assert.equal(E.pollStep(s, { type: 'unknown' }).delay, null);
  assert.deepEqual(E.queueProgress(empty), null);
  assert.deepEqual(E.queueProgress({ ...active, errors_since_start: 1 }), { running: 'hub:a', waiting: 3, done: 0, errors: 1 });
});

test('run requests: exactly one of subjects / select; force only when asked', () => {
  assert.deepEqual(E.runBody({ subjects: ['hub:a'], tiers: 'static' }), { subjects: ['hub:a'], tiers: 'static' });
  assert.deepEqual(E.runBody({ select: 'none_or_stale', source: 'hermes', tiers: 'tier1', force: true }), { select: 'none_or_stale', source: 'hermes', tiers: 'tier1', force: true });
  assert.deepEqual(E.runBody({ tiers: 'static' }), { select: 'none_or_stale', source: 'hub', tiers: 'static' });
  assert.deepEqual(E.runOutcome({ queued: ['a', 'b'], skipped: [{}] }), { queued: 2, skipped: 1 });
  assert.deepEqual(E.runOutcome(null), { queued: 0, skipped: 0 });
});

test('import form: override reason only matters for blocked items; names are validated and unique; body is minimal', () => {
  const sel = [rows[1], rows[2], { ...rows[2], subject: 'h:k9', name: 'notes2' }];
  const items = E.importDraft(sel);
  assert.deepEqual(items.map((i) => [i.key, i.gate]), [['k1', 'blocked'], ['k2', 'missing'], ['k9', 'missing']]);
  assert.equal(E.needsOverride(items), true);
  assert.equal(E.validateImport(items, { override: '' })[0].key, 'ev.imp_err_reason');
  assert.equal(E.validateImport(items, { override: 'too short' }).length, 1);
  assert.equal(E.validateImport(items, { override: 'x'.repeat(501) }).length, 1);
  assert.deepEqual(E.validateImport(items, { override: 'Reviewed by hand; the pipe is pinned' }), []);
  const ok = E.importDraft([rows[2]]);
  assert.equal(E.needsOverride(ok), false); assert.deepEqual(E.validateImport(ok), []);
  ok[0].finalName = 'Bad Name'; assert.equal(E.validateImport(ok)[0].key, 'ev.imp_err_name');
  const dup = E.importDraft([rows[2], { ...rows[2], subject: 'h:k9' }]); assert.equal(E.validateImport(dup)[0].key, 'ev.imp_err_dup');
  assert.equal(E.validateImport([])[0].key, 'ev.imp_err_none');
  // body: enable defaults to false, name only when changed, reason only when needed
  assert.deepEqual(E.importBody(ok.map((i) => ({ ...i, finalName: i.name })), {}), { items: [{ key: 'k2' }], on_conflict: 'skip', enable: false });
  const b = E.importBody(items.map((i, n) => (n ? i : { ...i, finalName: 'renamed' })), { onConflict: 'rename', enable: true, override: '  reviewed by hand  ' });
  assert.deepEqual(b, { items: [{ key: 'k1', name: 'renamed' }, { key: 'k2' }, { key: 'k9' }], on_conflict: 'rename', enable: true, override_reason: 'reviewed by hand' });
  assert.ok(!('override_reason' in E.importBody(ok.map((i) => ({ ...i, finalName: i.name })), { override: 'ignored when nothing is blocked' })));
});

test('import results accept {results}, {items} or a bare array and tally by status', () => {
  const r = { key: 'k', final_name: 'x-2', status: 'renamed', gate: 'overridden', detail: 'd' };
  assert.deepEqual(E.importResults({ results: [r] })[0], { key: 'k', finalName: 'x-2', status: 'renamed', gate: 'overridden', detail: 'd', code: '' });
  assert.equal(E.importResults([r]).length, 1); assert.equal(E.importResults({ items: [r] }).length, 1); assert.deepEqual(E.importResults({}), []);
  assert.deepEqual(E.importTally(E.importResults([r, { ...r, status: 'refused' }, { ...r, status: 'refused' }])), { renamed: 1, refused: 2 });
});

test('observed card model: both response shapes, origin order fixed, zero hub counts hidden', () => {
  assert.equal(E.observedModel(null), null);
  const post = E.observedModel({ observed_at: 5, complete: true, total: 10, by_origin: { agent: 3, shipped: 7 }, by_hub_status: { in_hub_same: 2, in_hub_differs: 0, not_in_hub: 8 }, notes: ['n'] });
  assert.deepEqual(post.byOrigin.map((o) => o.origin), ['shipped', 'agent']);
  assert.deepEqual(post.byHub, [{ status: 'in_hub_same', count: 2 }, { status: 'not_in_hub', count: 8 }]);
  const summary = E.observedModel({ observed_at: 5, total: 10, not_in_hub: 6, in_hub_differs: 1, by_origin: { client: 10 } });
  assert.deepEqual(summary.byHub, [{ status: 'in_hub_same', count: 3 }, { status: 'in_hub_differs', count: 1 }, { status: 'not_in_hub', count: 6 }]);
  assert.equal(summary.complete, true); assert.equal(E.observedModel({ total: 0, complete: false }).complete, false);
  assert.equal(E.evalLink('claude code'), '#/evaluations?source=claude%20code');
});

test('library index accepts old {skill} items and new subject items, never observed ones', () => {
  const idx = evalIndex([{ skill: 'a', state: 'current' }, { subject: 'hub:b', source: 'hub', name: 'b' }, { subject: 'h:k', source: 'h', name: 'c' }]);
  assert.deepEqual(Object.keys(idx).sort(), ['a', 'b']);
});

test('security crit/high counts only security validators; unknown means a dash, never the all-validator counts', () => {
  const v = (validator, severity) => ({ validator, status: 'failed', severity });
  // the publication schema finding has two highs but the gate (and this column) ignore it
  const rec = { tier1: { severity: { critical: 0, high: 2 } }, validators: [v('Schema & Repository Governance', { high: 2 }), v('QUALITY', { high: 1 }), v('Prompt Injection', { critical: 1, high: 1 }), v('Brand New Scanner', { high: 2 })] };
  assert.deepEqual(E.securitySeverity(rec), { critical: 1, high: 3 });
  assert.deepEqual(E.securitySeverity({ ...rec, security_findings: [{ severity: 'high' }, { severity: 'medium' }, { severity: 'critical' }, { severity: 'critical' }] }), { critical: 2, high: 1 });   // findings win
  assert.deepEqual(E.securitySeverity({ security_findings: [] }), { critical: 0, high: 0 });
  assert.deepEqual(E.securitySeverity({ security_severity: { critical: 0, high: 4 } }), { critical: 0, high: 4 });
  assert.equal(E.securitySeverity({ tier1: { severity: { high: 2 } } }), null);
  assert.equal(E.securitySeverity(null), null);
  const none = E.rowNumbers({ latest: { tier1: { severity: { high: 2 } }, validators: [v('Schema & Repository Governance', { high: 2 })] } });
  assert.deepEqual([none.critical, none.high], [0, 0]);   // gate-ok skill no longer reads "0 / 2"
});
