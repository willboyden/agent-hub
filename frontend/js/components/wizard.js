// Minimal multi-step wizard: ordered list (aria-current=step), Back/Next buttons, per-step validation, final action.
// steps: [{id, title, render: () => Node, valid?: () => true | string (error text), onEnter?: () => void}]
import { h } from '../dom.js';
import { t } from '../i18n.js';

export function wizard({ steps, onFinish, finishLabel = t('common.finish'), busyLabel = t('common.working') }) {
  let i = 0, busy = false;
  const list = h('ol', { class: 'steps', 'aria-label': t('wizard.steps') });
  const body = h('div', { class: 'wiz-body' });
  const err = h('div', { class: 'perr wiz-err', role: 'alert' });
  const back = h('button', { type: 'button', class: 'btn', onClick: () => go(i - 1) }, t('common.back'));
  const next = h('button', { type: 'button', class: 'btn primary', onClick: () => advance() }, t('common.next'));
  const root = h('div', { class: 'wizard' }, list, body, err, h('div', { class: 'row gap end wiz-nav' }, back, next));
  const paint = () => {
    list.replaceChildren(...steps.map((s, k) => h('li', { class: k < i ? 'done' : k === i ? 'now' : '', 'aria-current': k === i ? 'step' : null }, h('span', { class: 'dot', 'aria-hidden': 'true' }, k < i ? '✓' : String(k + 1)), h('span', s.title))));
    body.replaceChildren(steps[i].render()); err.textContent = '';
    back.hidden = i === 0; next.textContent = i === steps.length - 1 ? finishLabel : t('common.next');
    steps[i].onEnter?.();
    body.querySelector('input, select, textarea, button')?.focus?.();
  };
  const go = (k) => { i = Math.max(0, Math.min(steps.length - 1, k)); paint(); };
  async function advance() {
    if (busy) return;
    const v = steps[i].valid ? steps[i].valid() : true;
    if (v !== true) { err.textContent = typeof v === 'string' ? v : t('wizard.incomplete'); return; }
    if (i < steps.length - 1) return go(i + 1);
    busy = true; next.disabled = true; next.textContent = busyLabel;
    try { await onFinish(); } catch (e) { err.textContent = e?.message || String(e); } finally { busy = false; next.disabled = false; next.textContent = finishLabel; }
  }
  paint();
  return Object.assign(root, { go, refresh: paint, get step() { return i; } });
}
