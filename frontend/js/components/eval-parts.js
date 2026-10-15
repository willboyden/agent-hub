// Small presentational pieces shared by the Evaluations view, its detail drawer, the import dialog and the Clients "Observed" card.
// Colour is only a hint: every badge carries an icon and the full word.
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';
import { gateModel, verdictCell, ORIGINS, hubStatusKey } from '../evalview.js';

export function gateBadge(gate, { withTip = true } = {}) {
  const m = gateModel(gate);
  if (!m) return h('span', { class: 'muted', title: t('ev.gate_off_tip') }, '-');
  return h('span', { class: `badge ${m.tone} ev-gate ev-gate-${m.status}`, title: withTip ? t(m.tipKey) : null }, icon(m.icon, 12), h('span', t(m.key)));
}
export function verdictBadge(row) {
  const c = verdictCell(row);
  return h('span', { class: `badge ${c.tone} ev-verdict ev-v-${c.kind}${c.stale ? ' ev-stale' : ''}` }, icon(c.kind === 'running' ? 'restart' : c.kind === 'passed' ? 'check' : c.kind === 'none' || c.kind === 'queued' ? 'minus' : 'warn', 12),
    h('span', t(c.key)), c.stale ? h('span', { class: 'ev-stale-tag' }, ` · ${t('ev.state_stale')}`) : null);
}
export const originBadge = (origin) => h('span', { class: `badge muted ev-origin ev-o-${origin}` }, ORIGINS.includes(origin) ? t(`ev.origin_${origin}`) : String(origin || '-'));
export function hubStatusBadge(status) {
  const k = hubStatusKey(status);
  return k ? h('span', { class: `badge ${status === 'in_hub_same' ? 'ok' : status === 'in_hub_differs' ? 'warn' : 'muted'}` }, t(k)) : null;
}
export const dash = () => h('span', { class: 'muted', 'aria-label': t('ev.no_value') }, '-');
