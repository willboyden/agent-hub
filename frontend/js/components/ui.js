// Small presentational helpers built on h(). Text always goes through i18n via t().
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';
import { describeError } from '../errors.js';

export const badge = (text, kind = 'muted', title) => h('span', { class: `badge ${kind}`, title }, text);
export function btn(label, { icon: ic, kind = '', size = '', onClick, title, type = 'button', disabled, ariaLabel, pressed } = {}) {
  return h('button', { type, class: `btn ${kind} ${size}`.trim(), onClick, title, disabled, 'aria-pressed': pressed == null ? null : String(pressed), 'aria-label': ariaLabel || (label ? null : title) },
    ic ? icon(ic, 16) : null, label ? h('span', label) : null);
}
export const card = (title, body, actions, cls = '') => h('section', { class: `card ${cls}`.trim() },
  title || actions ? h('header', { class: 'card-head' }, title ? h('h2', title) : h('span'), actions ? h('div', { class: 'row gap' }, actions) : null) : null,
  h('div', { class: 'card-body' }, body));

export const KIND_ICON = { skill: 'star', agent: 'terminal', instruction: 'list', mcp: 'server', rule: 'shield', memory: 'knowledge' };
export const kindBadge = (kind) => h('span', { class: `badge kind kind-${kind}` }, icon(KIND_ICON[kind] || 'file', 12), h('span', t(`kind.${kind}`)));

const STATUS_KIND = { in_sync: 'ok', drift: 'warn', edited_outside: 'warn', pending_changes: 'info', never_applied: 'muted', error: 'bad', clean: 'ok', findings: 'bad', unscanned: 'warn', applied: 'ok', blocked: 'bad', partial: 'warn' };
export const statusBadge = (s) => badge(t(`status.${s}`), STATUS_KIND[s] || 'muted');

export const skeleton = (n = 3) => h('div', { class: 'skeleton-group', 'aria-busy': 'true', 'aria-label': t('common.loading') },
  Array.from({ length: n }, () => h('div', { class: 'skeleton' })));
export function errorBox(err, retry) {
  return h('div', { class: 'state error', role: 'alert' }, icon('warn', 22),
    h('div', h('strong', err?.title || t('common.error')), h('p', describeError(err))),
    err?.status === 401 ? h('p', { class: 'hint' }, t('auth.required')) : null,
    retry ? btn(t('common.retry'), { icon: 'restart', onClick: retry }) : null);
}
export const emptyBox = (title, hint, action) => h('div', { class: 'state empty' }, h('strong', title), hint ? h('p', hint) : null, action || null);

let uid = 0;
export function field(label, control, hint) {
  const id = control.id || `f${++uid}`;
  control.id = id;
  return h('div', { class: 'field' }, h('label', { for: id }, label), control, hint ? h('div', { class: 'hint', id: `${id}-h` }, hint) : null);
}
export function select(options, value, onChange, attrs = {}) {
  return h('select', { ...attrs, onChange: (e) => onChange?.(e.target.value) },
    options.map((o) => { const v = typeof o === 'string' ? { value: o, label: o } : o; return h('option', { value: v.value, selected: v.value === value }, v.label); }));
}
export function chip(text, { onRemove, kind = '', title } = {}) {
  return h('span', { class: `chip ${kind}`.trim(), title }, h('span', text),
    onRemove ? h('button', { type: 'button', class: 'chip-x', 'aria-label': t('common.remove_named', { name: text }), onClick: onRemove }, icon('close', 12)) : null);
}
// Roving-tabindex tab strip (arrow keys move, Home/End jump). `panels` is not managed here; callers swap content on change.
export function tabs(items, active, onChange, label) {
  const bar = h('div', { class: 'tabs', role: 'tablist', 'aria-label': label || null });
  const paint = (id) => bar.querySelectorAll('[role=tab]').forEach((b) => { const on = b.dataset.id === id; b.classList.toggle('on', on); b.setAttribute('aria-selected', String(on)); b.tabIndex = on ? 0 : -1; });
  for (const it of items) bar.append(h('button', { type: 'button', role: 'tab', class: 'tab', dataset: { id: it.id }, id: `tab-${it.id}`, onClick: () => { paint(it.id); onChange(it.id); } }, it.label, it.count != null ? h('span', { class: 'tab-count' }, String(it.count)) : null));
  bar.addEventListener('keydown', (e) => {
    const tabsEls = [...bar.querySelectorAll('[role=tab]')]; const i = tabsEls.indexOf(document.activeElement);
    if (i < 0) return; let j = null;
    if (e.key === 'ArrowRight') j = (i + 1) % tabsEls.length; else if (e.key === 'ArrowLeft') j = (i - 1 + tabsEls.length) % tabsEls.length; else if (e.key === 'Home') j = 0; else if (e.key === 'End') j = tabsEls.length - 1;
    if (j != null) { e.preventDefault(); tabsEls[j].focus(); tabsEls[j].click(); }
  });
  paint(active);
  return bar;
}
export function seg(options, value, onChange, label) {
  const wrap = h('div', { class: 'seg-control', role: 'group', 'aria-label': label });
  const paint = (v) => wrap.querySelectorAll('button').forEach((b) => { const on = b.dataset.v === v; b.classList.toggle('on', on); b.setAttribute('aria-pressed', String(on)); });
  for (const o of options) wrap.append(h('button', { type: 'button', dataset: { v: o.value }, title: o.title || null, onClick: () => { paint(o.value); onChange(o.value); } }, o.icon ? icon(o.icon, 16) : null, h('span', o.label)));
  paint(value);
  return wrap;
}
export const kv = (k, v) => h('div', { class: 'kv' }, h('dt', k), h('dd', v));
export const timeText = (ago) => (ago ? t(`time.${ago.u}`, { n: ago.n }) : t('common.never'));
// Fires a bubbling event views listen to when content changed (nav badges, cached lists refresh).
export const notifyChanged = () => window.dispatchEvent(new Event('ah:changed'));
export function bar(pct, label, kind = '') {
  const p = Math.max(0, Math.min(100, Number.isFinite(pct) ? pct : 0));
  const fill = h('div', { class: `fill ${kind}` }); fill.style.width = `${p}%`;
  return h('div', { class: 'bar', role: 'progressbar', 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': Math.round(p), 'aria-label': label }, fill);
}

// Client status badge: pending_changes carries the file count ("Hub would change N files").
export const clientStatusBadge = (c) => (c.status === 'pending_changes' ? badge(t('status.pending_changes', { count: c.drift?.pending_changes ?? 0 }), 'info') : statusBadge(c.status));
// Paths edited outside the hub, and why (from client.drift.edited_outside).
export function driftDetails(c) {
  const list = c.drift?.edited_outside || [];
  if (!list.length) return null;
  return h('details', { class: 'drift-list' }, h('summary', t('cl.drift_details', { count: list.length })),
    h('ul', list.map((f) => h('li', h('code', f.root ? `${f.root}: ${f.path}` : f.path), f.reason ? h('div', { class: 'muted small' }, f.reason) : null))));
}
