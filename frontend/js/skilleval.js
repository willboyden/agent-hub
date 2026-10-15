// SkillEvaluator results (advisory) for the Library badge and the editor's Evaluation panel. Pure logic, no DOM: node-tested.
// Text is produced through a translator passed in (`t`), so these helpers never hard-code a user-visible string.
export const EVAL_TIERS = ['static', 'tier1', 'tier1+2'];
export const VERDICTS = ['passed', 'failed', 'incomplete', 'error'];
const TONE = { passed: 'ok', incomplete: 'warn', failed: 'bad', error: 'bad' };
const ICON = { passed: 'check', incomplete: 'warn', failed: 'warn', error: 'warn' };
export const SEVERITIES = ['critical', 'high', 'medium', 'low'];

export const verdictOf = (rec) => (VERDICTS.includes(rec?.verdict) ? rec.verdict : 'error');
export const verdictTone = (v) => TONE[v] || 'muted';
// i18n keys may not contain '+', so the tier id is mapped to a key-safe suffix.
export const tierKey = (tier) => String(tier || '').replace('+', '_');
// One decimal at most, never "NaN": 83 -> "83", 83.64 -> "83.6"; null when the report had no number.
export function fmtScore(n) { return typeof n === 'number' && Number.isFinite(n) ? String(Math.round(n * 10) / 10) : null; }
export const evalIndex = (items) => Object.fromEntries((Array.isArray(items) ? items : []).filter((e) => e && e.skill).map((e) => [e.skill, e]));

// The library badge for one skill. `entry` is one item of GET /skill-evaluations ({skill, state, latest, running}).
// state: running | none | current | stale. Colour is only a hint: the text always names the verdict and the state.
export function badgeModel(entry) {
  if (!entry) return { state: 'none', tone: 'muted', icon: 'minus', verdict: null, grade: null, score: null, judge: null };
  if (entry.running) return { state: 'running', tone: 'info', icon: 'restart', verdict: null, grade: null, score: null, judge: null };
  const L = entry.latest;
  if (!L || entry.state === 'none') return { state: 'none', tone: 'muted', icon: 'minus', verdict: null, grade: null, score: null, judge: null };
  const verdict = verdictOf(L); const t1 = L.tier1 || {};
  const stale = entry.state !== 'current';
  return { state: stale ? 'stale' : 'current', tone: stale ? 'muted' : verdictTone(verdict), icon: stale ? 'history' : ICON[verdict],
    verdict, grade: typeof t1.grade === 'string' && t1.grade ? t1.grade : null, score: fmtScore(t1.quality_score), judge: fmtScore(t1.judge_score) };
}

// {short, long}: short is the visible badge text ("B 83 · passed", "not evaluated", "B 83 · stale"); long is the full sentence for
// the accessible name and tooltip.
export function badgeText(m, t) {
  if (m.state === 'running') return { short: t('ev.badge_running'), long: t('ev.badge_running_long') };
  if (m.state === 'none') return { short: t('ev.badge_none'), long: t('ev.badge_none_long') };
  const score = [m.grade, m.score].filter(Boolean).join(' ');
  const verdict = t(`ev.verdict_${m.verdict}`);
  const short = [score || null, m.state === 'stale' ? t('ev.state_stale') : verdict].filter(Boolean).join(' · ');
  const parts = [t('ev.badge_long', { verdict })];
  if (m.grade || m.score) parts.push(t('ev.badge_long_quality', { grade: m.grade || '-', score: m.score ?? '-' }));
  if (m.judge) parts.push(t('ev.badge_long_judge', { score: m.judge }));
  parts.push(t(m.state === 'stale' ? 'ev.badge_long_stale' : 'ev.badge_long_current'));
  return { short, long: parts.join(' ') };
}

// Facts for the latest run, in display order. Missing values are left out (never invented).
export function summaryFacts(rec) {
  if (!rec) return [];
  const t1 = rec.tier1 || {}; const sev = t1.severity || {};
  const rows = [
    ['ev.f_tiers', rec.tiers ? { key: `ev.tier_${tierKey(rec.tiers)}` } : null],
    ['ev.f_quality', t1.quality_score != null ? `${fmtScore(t1.quality_score)}${t1.grade ? ` (${t1.grade})` : ''}` : null],
    ['ev.f_judge', fmtScore(t1.judge_score) ?? (t1.judge_status && t1.judge_status !== 'succeeded' ? { key: 'ev.judge_not_run', vars: { status: t1.judge_status } } : null)],
    ['ev.f_errors', t1.errors != null || t1.warnings != null ? `${t1.errors ?? '-'} / ${t1.warnings ?? '-'}` : null],
    ['ev.f_severity', SEVERITIES.some((s) => sev[s] != null) ? SEVERITIES.map((s) => `${s} ${sev[s] ?? '-'}`).join(', ') : null],
    ['ev.f_incomplete', (t1.incomplete || []).length ? t1.incomplete.join(', ') : null],
    ['ev.f_tier2', rec.tier2 ? { key: `ev.verdict_${verdictOf({ verdict: rec.tier2.status })}` } : null],
    ['ev.f_duration', Number.isFinite(rec.duration_ms) ? `${(rec.duration_ms / 1000).toFixed(1)} s` : null],
    ['ev.f_launcher', rec.launcher || null],
    ['ev.f_files', Number.isFinite(rec.files) ? `${rec.files} (${rec.bytes ?? 0} B)` : null],
    ['ev.f_hash', typeof rec.content_hash === 'string' ? rec.content_hash.slice(0, 12) : null],
  ];
  return rows.filter(([, v]) => v != null && v !== '').map(([key, value]) => ({ key, value }));
}

// Medium-and-above findings as rows, most severe first (the server already sorted and capped them).
export const findingRows = (rec) => (Array.isArray(rec?.findings) ? rec.findings : []).filter((f) => f && typeof f.message === 'string')
  .map((f) => ({ severity: SEVERITIES.includes(f.severity) ? f.severity : 'medium', validator: String(f.validator || '?'), message: f.message }));

// The run we started has finished once the server no longer reports it as running and the history holds it.
export function runFinished(detail, runId) {
  if (!detail) return false;
  if (detail.running && detail.running.run_id === runId) return false;
  return (detail.history || []).some((h) => h.run_id === runId) || !detail.running;
}
// Poll quickly at first (static takes about 2 s), then back off to every 5 s.
export const pollDelay = (attempt) => Math.min(5000, 1000 + 500 * Math.max(0, attempt));
