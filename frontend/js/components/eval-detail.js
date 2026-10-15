// Detail drawer body for one evaluation subject (F11/F14): gate + verdict, tier summaries, per-validator tables by pass, top findings,
// the Markdown report (sanitising renderer only) and the run history. Observed rows get "Import into hub...".
import { h, clear, setTrustedHtml } from '../dom.js';
import { t } from '../i18n.js';
import { renderMarkdown } from '../markdown.js';
import { btn, badge, kv, skeleton, errorBox, timeText } from './ui.js';
import { gateBadge, verdictBadge, originBadge, hubStatusBadge } from './eval-parts.js';
import { summaryFacts, findingRows, verdictTone, verdictOf, tierKey } from '../skilleval.js';
import { validatorPasses, passLabelKey, isObserved, gateStatus } from '../evalview.js';
import { timeAgo } from '../util.js';

const factValue = (v) => (v && typeof v === 'object' ? t(v.key, v.vars) : String(v));
const when = (ts) => (ts ? timeText(timeAgo(ts)) : t('common.never'));

function validatorTable(pass, both) {
  const label = t(passLabelKey(pass.pass, both));
  const head = ['ev.col_validator', 'ev.col_status', 'ev.sev_critical', 'ev.sev_high', 'ev.sev_medium'].map((k) => h('th', { scope: 'col' }, t(k)));
  const rows = pass.rows.map((v) => h('tr', { class: v.security ? 'ev-sec' : '' },
    h('td', v.validator, v.security ? null : h('span', { class: 'muted small' }, ` (${t('ev.not_gating')})`)),
    h('td', badge(t(`ev.verdict_${v.status}`), verdictTone(v.status))),
    ...['critical', 'high', 'medium'].map((s) => h('td', { class: v.sev[s] ? 'ev-num hot' : 'ev-num' }, String(v.sev[s])))));
  return h('section', { class: 'ev-pass' }, h('h4', label), h('div', { class: 'tablewrap' }, h('table', { class: 'compact ev-vtable' }, h('caption', { class: 'sr-only' }, label), h('thead', h('tr', head)), h('tbody', rows))));
}

export function detailBody(d, { api, tiersPicker, onEvaluate, onImport, gateMode }) {
  const latest = d.latest;
  const row = { ...d, queued: false, running: !!d.running };
  const gate = d.gate ?? null;
  const reasons = Array.isArray(gate?.reasons) ? gate.reasons : [];
  const report = h('div', { class: 'ev-report', role: 'region', 'aria-live': 'polite', 'aria-label': t('ev.report') });
  const showReport = async (runId, trigger) => {
    clear(report).append(skeleton(3));
    try {
      const r = await api.evalRunReport(d.subject, runId);
      clear(report);
      if (!r.markdown) { report.append(h('p', { class: 'muted' }, t(r.truncated ? 'ev.report_truncated' : 'ev.report_none'))); return; }
      const body = h('div', { class: 'md-preview' }); setTrustedHtml(body, renderMarkdown(r.markdown, { copyLabel: t('common.copy') }));
      report.append(h('p', { class: 'muted small' }, t('ev.report_run', { run: runId })), body, r.truncated ? h('p', { class: 'note warn' }, t('ev.report_truncated')) : null);
      trigger?.focus?.();
    } catch (e) { clear(report).append(errorBox(e, () => showReport(runId, trigger))); }
  };
  const facts = summaryFacts(latest);
  const passes = validatorPasses(latest);
  const findings = findingRows(latest);
  const hist = Array.isArray(d.history) ? d.history : [];
  const observed = isObserved(d);
  return h('div', { class: 'stack-v ev-detail' },
    h('div', { class: 'row gap wrap' }, h('code', { class: 'ev-subject' }, d.subject), originBadge(d.origin), observed ? hubStatusBadge(d.hub_status) : null),
    h('p', { class: 'muted small' }, t('ev.where', { source: d.source, path: d.path || '-' })),
    h('div', { class: 'row gap wrap' }, gateBadge(gate), verdictBadge(row), h('span', { class: 'muted small' }, latest ? t('ev.evaluated_when', { when: when(latest.finished_at) }) : t('ev.never_evaluated'))),
    gateStatus(gate) ? h('p', { class: 'hint ev-gate-note' }, t('ev.gate_note')) : null,
    reasons.length ? h('ul', { class: `ev-reasons note ${gateStatus(gate) === 'blocked' ? 'bad' : 'warn'}` }, reasons.map((x) => h('li', x))) : null,
    latest?.reused_from ? h('p', { class: 'note info' }, t('ev.reused', { subject: latest.reused_from.subject })) : null,
    h('div', { class: 'row gap wrap ev-actions' }, tiersPicker, btn(t('ev.evaluate_again'), { icon: 'restart', kind: 'primary', onClick: () => onEvaluate(d) }),
      observed && onImport ? btn(t('ev.import_btn'), { icon: 'import', onClick: () => onImport(d) }) : null),
    latest?.error ? h('p', { class: 'note bad', role: 'alert' }, t('ev.run_error', { error: latest.error })) : null,
    h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h3', t('ev.tier_summaries'))),
      h('div', { class: 'card-body' }, facts.length ? h('dl', { class: 'facts' }, facts.map((f) => kv(t(f.key), factValue(f.value)))) : h('p', { class: 'muted' }, t('ev.no_facts')))),
    passes.length ? h('section', { class: 'stack-v' }, h('h3', t('ev.validators')), passes.map((p) => validatorTable(p, passes.length > 1))) : null,
    h('section', { class: 'stack-v tight' }, h('h3', t('ev.top_findings')),
      findings.length ? h('ul', { class: 'findings ev-findings' }, findings.map((f) => h('li', { class: `note ${f.severity === 'medium' ? 'warn' : 'bad'}` }, h('strong', t(`ev.sev_${f.severity}`)), ' ', h('span', { class: 'muted' }, f.validator), ': ', f.message)))
        : h('p', { class: 'muted' }, t('ev.no_findings')),
      latest?.findings_total > findings.length ? h('p', { class: 'muted small' }, t('ev.findings_more', { count: latest.findings_total - findings.length })) : null),
    h('section', { class: 'stack-v tight' }, h('div', { class: 'row between gap wrap' }, h('h3', t('ev.report')), latest ? btn(t('ev.show_report'), { icon: 'file', size: 'sm', onClick: (e) => showReport(latest.run_id, e.currentTarget) }) : null), report),
    h('section', { class: 'stack-v tight' }, h('h3', t('ev.history')),
      hist.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('caption', { class: 'sr-only' }, t('ev.history')),
        h('thead', h('tr', ['ev.col_run', 'ev.col_tiers', 'ev.col_verdict', 'ev.col_evaluated', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('ev.report')))))),
        h('tbody', hist.map((r) => h('tr', h('td', h('code', String(r.run_id).slice(0, 10)), r.reused_from ? h('span', { class: 'muted small' }, ` ${t('ev.reused_tag')}`) : null),
          h('td', t(`ev.tier_${tierKey(r.tiers)}`)), h('td', badge(t(`ev.verdict_${verdictOf(r)}`), verdictTone(verdictOf(r))), r.state === 'stale' ? h('span', { class: 'muted small' }, ` ${t('ev.state_stale')}`) : null),
          h('td', { class: 'muted nowrap' }, when(r.finished_at)),
          h('td', { class: 'row' }, btn(t('ev.show_report'), { size: 'sm', onClick: (e) => showReport(r.run_id, e.currentTarget) }))))))) : h('p', { class: 'muted' }, t('ev.no_history'))),
    gateMode === 'off' ? h('p', { class: 'hint' }, t('ev.gate_off_note')) : null);
}
