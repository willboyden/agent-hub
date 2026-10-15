// Pure logic for the Evaluations view (F9/F11/F14): filters, summary counts, gate/verdict badges, per-pass validator tables, the
// queue-poll state machine and the observed-import form rules. No DOM, no fetch: node-tested. Every user-visible word is an i18n KEY.
import { VERDICTS, SEVERITIES, verdictTone, verdictOf, fmtScore, tierKey } from './skilleval.js';
import { NAME_RE } from './util.js';

export const ORIGINS = ['client', 'shipped', 'agent', 'lab_sync', 'hub', 'archived'];
export const STATES = ['none', 'current', 'stale'];
export const GATES = ['ok', 'warn', 'blocked', 'missing'];
export const GATE_MODES = ['off', 'advisory', 'require'];
export const MIN_TIERS = ['static', 'tier1', 'tier1+2'];   // Tier 3 is out of scope (F14)
export const CONFLICTS = ['skip', 'rename', 'replace', 'link'];
export const EMPTY_EV_FILTERS = { source: 'hub', origin: '', state: '', verdict: '', gate: '', q: '' };
export const OVERRIDE_MIN = 10;
export const OVERRIDE_MAX = 500;

export const tierRank = (t) => Math.max(0, MIN_TIERS.indexOf(t));
// Tier ids the picker offers: the evaluator's list when it reports one, restricted to the three that exist.
export const offeredTiers = (status) => {
  const ids = (Array.isArray(status?.tiers) ? status.tiers : []).map((x) => (typeof x === 'string' ? x : x?.id)).filter((id) => MIN_TIERS.includes(id));
  return ids.length ? ids : MIN_TIERS;
};

// ---- subjects: `hub:<skill>` or `<client>:<key>` ----
export function splitSubject(subject) {
  const s = String(subject ?? ''); const i = s.indexOf(':');
  return i < 0 ? { source: '', key: s } : { source: s.slice(0, i), key: s.slice(i + 1) };
}
export const isObserved = (row) => !!row && row.source !== 'hub' && !String(row.subject || '').startsWith('hub:');

// ---- filters ----
export function filtersFromQuery(query) {
  const o = { ...EMPTY_EV_FILTERS };
  for (const k of Object.keys(o)) if (typeof query?.[k] === 'string') o[k] = query[k];
  if (!o.source) o.source = 'hub';
  return o;
}
// The URL query for a filter set: defaults are left out so shared links stay short.
export const filtersToQuery = (f) => Object.fromEntries(Object.entries(f).filter(([k, v]) => v && v !== EMPTY_EV_FILTERS[k]));
export const activeEvFilterCount = (f) => ['origin', 'state', 'verdict', 'gate', 'q'].filter((k) => f[k]).length;

export const gateStatus = (g) => (typeof g === 'string' ? g : g && typeof g === 'object' ? g.status || null : null);
export const rowVerdict = (row) => (row?.latest ? verdictOf(row.latest) : null);
const hay = (r) => [r.name, r.path, r.subject, r.origin, r.source].filter(Boolean).join(' ').toLowerCase();

export function filterEvalRows(rows, f) {
  const o = { ...EMPTY_EV_FILTERS, ...f }; const q = o.q.trim().toLowerCase();
  return (rows || []).filter((r) => {
    if (o.origin && r.origin !== o.origin) return false;
    if (o.state && (r.state || 'none') !== o.state) return false;
    if (o.verdict && rowVerdict(r) !== o.verdict) return false;
    if (o.gate && (gateStatus(r.gate) || '') !== o.gate) return false;
    return !q || q.split(/\s+/).every((w) => hay(r).includes(w));
  });
}

// ---- summary chips ----
export function summarizeRows(rows) {
  const s = { total: 0, by_state: {}, by_verdict: {}, by_gate: {}, queued: 0, running: 0 };
  const bump = (m, k) => { m[k] = (m[k] || 0) + 1; };
  for (const r of rows || []) {
    s.total++; bump(s.by_state, r.state || 'none');
    const v = rowVerdict(r); if (v) bump(s.by_verdict, v);
    const g = gateStatus(r.gate); if (g) bump(s.by_gate, g);
    if (r.queued) s.queued++; if (r.running) s.running++;
  }
  return s;
}
// Chips shown above the table, in a fixed order, zero counts left out. Each chip sets one filter.
export function summaryChips(s) {
  const out = [];
  for (const g of GATES) if (s.by_gate[g]) out.push({ filter: 'gate', value: g, count: s.by_gate[g], key: `ev.gate_${g}` });
  for (const v of VERDICTS) if (s.by_verdict[v]) out.push({ filter: 'verdict', value: v, count: s.by_verdict[v], key: `ev.verdict_${v}` });
  if (s.by_state.none) out.push({ filter: 'state', value: 'none', count: s.by_state.none, key: 'ev.state_none' });
  if (s.by_state.stale) out.push({ filter: 'state', value: 'stale', count: s.by_state.stale, key: 'ev.state_stale' });
  return out;
}

// ---- badges ----
const GATE_TONE = { ok: 'ok', warn: 'warn', blocked: 'bad', missing: 'muted', overridden: 'warn' };
const GATE_ICON = { ok: 'check', warn: 'warn', blocked: 'lock', missing: 'minus', overridden: 'warn' };
export function gateModel(g) {
  const status = gateStatus(g);
  if (!status) return null;                                   // eval_gate=off: the server sends no gate
  return { status, tone: GATE_TONE[status] || 'muted', icon: GATE_ICON[status] || 'minus', key: `ev.gate_${status}`, tipKey: `ev.gate_${status}_tip` };
}
// The verdict cell: what is happening now beats the stored verdict; a stale verdict says so.
export function verdictCell(row) {
  if (row.running) return { kind: 'running', tone: 'info', key: 'ev.badge_running' };
  if (row.queued) return { kind: 'queued', tone: 'info', key: 'ev.state_queued' };
  const v = rowVerdict(row);
  if (!v || row.state === 'none') return { kind: 'none', tone: 'muted', key: 'ev.badge_none' };
  return { kind: v, tone: row.state === 'stale' ? 'muted' : verdictTone(v), key: `ev.verdict_${v}`, stale: row.state === 'stale' };
}
// Numbers for the table; null when the report had none (rendered as a dash, never as 0).
export function rowNumbers(row) {
  const t1 = row?.latest?.tier1 || {};
  const sec = securitySeverity(row?.latest);
  return { quality: t1.quality_score != null ? [t1.grade, fmtScore(t1.quality_score)].filter(Boolean).join(' ') : null, judge: fmtScore(t1.judge_score),
    critical: sec ? sec.critical : null, high: sec ? sec.high : null, at: row?.latest?.finished_at ?? null };
}
// Critical/high counts of SECURITY validators only (the gate ignores publication findings such as a missing author). Sources, in order:
// `security_findings` (counted), `validators[].severity` minus the non-security list, then `security_severity` {critical, high} if the
// backend sends it on list rows. `tier1.severity` counts every validator, so it is never used: null means "not known", shown as a dash.
export function securitySeverity(rec) {
  if (!rec) return null;
  const n = (x) => (Number.isFinite(x) ? x : 0);
  if (Array.isArray(rec.security_findings)) {
    const c = { critical: 0, high: 0 };
    for (const f of rec.security_findings) if (f && (f.severity === 'critical' || f.severity === 'high')) c[f.severity]++;
    return c;
  }
  if (Array.isArray(rec.validators)) {
    const c = { critical: 0, high: 0 };
    for (const v of rec.validators) if (v && isSecurityValidator(v.validator)) { c.critical += n(v.severity?.critical); c.high += n(v.severity?.high); }
    return c;
  }
  const s = rec.security_severity;
  return s && typeof s === 'object' ? { critical: n(s.critical), high: n(s.high) } : null;
}
export const hubStatusKey = (s) => (['in_hub_same', 'in_hub_differs', 'not_in_hub'].includes(s) ? `ev.hub_${s}` : null);

// ---- sorting ----
const GATE_ORDER = { blocked: 0, warn: 1, missing: 2, ok: 3 };
const VERDICT_ORDER = { failed: 0, error: 1, incomplete: 2, passed: 3 };
export const SORTS = ['name', 'gate', 'verdict', 'quality', 'evaluated'];
export function sortEvalRows(rows, key = 'name') {
  const cmpName = (a, b) => String(a.name).localeCompare(String(b.name)) || String(a.subject).localeCompare(String(b.subject));
  const num = (r) => { const q = r.latest?.tier1?.quality_score; return Number.isFinite(q) ? q : Infinity; };   // unscored rows go last
  const by = { name: cmpName,
    gate: (a, b) => (GATE_ORDER[gateStatus(a.gate)] ?? 9) - (GATE_ORDER[gateStatus(b.gate)] ?? 9) || cmpName(a, b),
    verdict: (a, b) => (VERDICT_ORDER[rowVerdict(a)] ?? 9) - (VERDICT_ORDER[rowVerdict(b)] ?? 9) || cmpName(a, b),
    quality: (a, b) => (num(a) === num(b) ? 0 : num(a) < num(b) ? -1 : 1) || cmpName(a, b),
    evaluated: (a, b) => (b.latest?.finished_at || 0) - (a.latest?.finished_at || 0) || cmpName(a, b) };
  return [...(rows || [])].sort(by[key] || cmpName);
}

// ---- detail: per-validator tables by pass (F14) ----
// The one place that knows the record's field names; the backend may still rename them.
const PUBLICATION = new Set(['Schema & Repository Governance', 'Semantic Version Validation', 'License Compliance', 'QUALITY', 'SCRIPT_LINT', 'RUBRIC_EVAL']);
export const isSecurityValidator = (name) => !PUBLICATION.has(name);     // unknown validators count as security (fail closed)
export function validatorPasses(rec) {
  if (!rec) return [];
  const norm = (list) => (Array.isArray(list) ? list : []).filter((v) => v && typeof v.validator === 'string').map((v) => {
    const sev = v.severity || v.severity_counts || {};
    return { validator: v.validator, status: ['passed', 'failed', 'incomplete'].includes(v.status) ? v.status : 'error', security: isSecurityValidator(v.validator),
      sev: Object.fromEntries(SEVERITIES.map((s) => [s, Number.isFinite(sev[s]) ? sev[s] : 0])) };
  });
  const stat = norm(rec.validators ?? rec.validators_static), llm = norm(rec.validators_llm);   // `validators` = the static pass
  const out = [];
  if (stat.length) out.push({ pass: 'static', rows: stat });
  if (llm.length) out.push({ pass: 'llm', rows: llm });
  return out;
}
// Pass headings are only needed when both passes exist.
export const passLabelKey = (pass, both) => (both ? `ev.pass_${pass}` : 'ev.validators');

// ---- queue poll state machine ----
export const POLL_MS = 2000, MAX_POLL_ERRORS = 3;
export const queueActive = (q) => !!q && (!!q.running || (q.queued_total ?? (q.queued || []).length) > 0);
export const pollInit = () => ({ phase: 'idle', errors: 0, finished: 0 });
// events: {type:'start'} (we just enqueued), {type:'queue', queue}, {type:'error'}, {type:'stop'}.
// Returns {state, delay, refresh}: delay is ms until the next poll (null = do not poll), refresh = reload the table now.
export function pollStep(s, ev) {
  switch (ev.type) {
    case 'start': return { state: { ...s, phase: 'polling', errors: 0 }, delay: 300, refresh: false };
    case 'stop': return { state: { ...pollInit(), finished: s.finished }, delay: null, refresh: false };
    case 'error': {
      const errors = s.errors + 1;
      return errors >= MAX_POLL_ERRORS ? { state: { ...s, phase: 'failed', errors }, delay: null, refresh: false } : { state: { ...s, errors }, delay: POLL_MS * errors, refresh: false };
    }
    case 'queue': {
      const q = ev.queue; const fin = q?.finished_since_start ?? s.finished;
      if (queueActive(q)) return { state: { phase: 'polling', errors: 0, finished: fin }, delay: POLL_MS, refresh: fin !== s.finished };
      // Empty queue: stop polling; if we were polling, the last jobs just finished, so refresh once.
      return { state: { phase: 'idle', errors: 0, finished: fin }, delay: null, refresh: s.phase === 'polling' };
    }
    default: return { state: s, delay: null, refresh: false };
  }
}
export const queueProgress = (q) => {
  if (!queueActive(q)) return null;
  return { running: q.running?.subject || null, waiting: q.queued_total ?? (q.queued || []).length, done: q.finished_since_start ?? 0, errors: q.errors_since_start ?? 0 };
};

// ---- run requests ----
export const runBody = ({ subjects, select, source, tiers, force }) => (subjects ? { subjects, tiers, ...(force ? { force: true } : {}) } : { select: select || 'none_or_stale', source: source || 'hub', tiers, ...(force ? { force: true } : {}) });
export const runOutcome = (r) => ({ queued: Array.isArray(r?.queued) ? r.queued.length : Number(r?.queued) || 0, skipped: Array.isArray(r?.skipped) ? r.skipped.length : 0 });

// ---- observed import (F8) ----
export const importableRows = (rows) => (rows || []).filter(isObserved);
export const clientsOf = (rows) => [...new Set(importableRows(rows).map((r) => r.source))];
export const needsOverride = (items) => (items || []).some((i) => gateStatus(i.gate) === 'blocked');
export function importDraft(rows) {
  return importableRows(rows).map((r) => ({ key: splitSubject(r.subject).key, subject: r.subject, name: r.name, finalName: r.name, gate: gateStatus(r.gate), origin: r.origin, path: r.path }));
}
// Returns i18n keys of the first problems found (empty = ready). `override` is only needed when an item is blocked.
export function validateImport(items, { override = '' } = {}) {
  const errs = []; const seen = new Set();
  if (!items.length) errs.push({ key: 'ev.imp_err_none' });
  for (const i of items) {
    if (!NAME_RE.test(i.finalName)) errs.push({ key: 'ev.imp_err_name', subject: i.subject });
    else if (seen.has(i.finalName)) errs.push({ key: 'ev.imp_err_dup', subject: i.subject });
    seen.add(i.finalName);
  }
  if (needsOverride(items)) { const n = String(override).trim().length; if (n < OVERRIDE_MIN || n > OVERRIDE_MAX) errs.push({ key: 'ev.imp_err_reason', min: OVERRIDE_MIN, max: OVERRIDE_MAX }); }
  return errs;
}
export function importBody(items, { onConflict = 'skip', enable = false, override = '' } = {}) {
  const body = { items: items.map((i) => (i.finalName && i.finalName !== i.name ? { key: i.key, name: i.finalName } : { key: i.key })), on_conflict: onConflict, enable: !!enable };
  if (needsOverride(items) && override.trim()) body.override_reason = override.trim();
  return body;
}
// The response shape for per-item results is not pinned down (F8 only lists the fields): accept {results}, {items} or a bare array.
export function importResults(resp) {
  const list = Array.isArray(resp) ? resp : Array.isArray(resp?.results) ? resp.results : Array.isArray(resp?.items) ? resp.items : [];
  return list.map((r) => ({ key: r.key, finalName: r.final_name ?? r.name ?? r.key, status: r.status || 'error', gate: gateStatus(r.gate), detail: r.detail || '', code: r.code || '' }));
}
export const IMPORT_TONE = { created: 'ok', replaced: 'ok', renamed: 'info', linked: 'info', skipped: 'muted', refused: 'bad', error: 'bad' };
export function importTally(results) { const t = {}; for (const r of results) t[r.status] = (t[r.status] || 0) + 1; return t; }

// ---- observed card (clients) ----
// Accepts both {by_hub_status} (POST observe) and {not_in_hub, in_hub_differs} (summary on /clients, /drift).
export function observedModel(s) {
  if (!s || typeof s !== 'object') return null;
  const total = Number(s.total) || 0;
  const hs = s.by_hub_status || {};
  const notIn = hs.not_in_hub ?? s.not_in_hub ?? 0, differs = hs.in_hub_differs ?? s.in_hub_differs ?? 0;
  const same = hs.in_hub_same ?? s.in_hub_same ?? Math.max(0, total - notIn - differs);
  const byOrigin = ORIGINS.filter((o) => s.by_origin?.[o]).map((o) => ({ origin: o, count: s.by_origin[o] }));
  return { observedAt: s.observed_at ?? null, total, complete: s.complete !== false, notes: Array.isArray(s.notes) ? s.notes : [], byOrigin,
    byHub: [['in_hub_same', same], ['in_hub_differs', differs], ['not_in_hub', notIn]].filter(([, n]) => n > 0).map(([status, count]) => ({ status, count })) };
}
export const evalLink = (source) => `#/evaluations?source=${encodeURIComponent(source)}`;
export { tierKey };
