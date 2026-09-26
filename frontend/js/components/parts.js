// Shared small building blocks: collection chips, client dots, issue badge, "new item" dialog.
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';
import { badge, field } from './ui.js';
import { openModal } from './dialog.js';
import { validName, slug, readableText } from '../util.js';
import { KINDS } from '../api.js';

export function colorDot(color, cls = 'swatch') { const i = h('i', { class: cls, 'aria-hidden': 'true' }); if (color) i.style.background = color; return i; }
export function collectionChip(c, { href = true } = {}) {
  const inner = [colorDot(c.color), h('span', c.title)];
  return href ? h('a', { class: 'chip coll', href: `#/collections/${encodeURIComponent(c.id)}`, title: c.title }, inner) : h('span', { class: 'chip coll' }, inner);
}
// A dot per client the item is enabled for. Colour is never the only cue: each dot shows the client's initial and has a text title.
export function clientDots(ids, clientsById) {
  const wrap = h('span', { class: 'cdots', role: 'img', 'aria-label': ids.length ? t('lib.enabled_for', { names: ids.map((i) => clientsById[i]?.display_name || i).join(', ') }) : t('lib.enabled_none') });
  if (!ids.length) { wrap.append(h('span', { class: 'muted small' }, t('lib.enabled_none_short'))); return wrap; }
  for (const id of ids) { const c = clientsById[id]; const d = h('span', { class: 'cdot', title: c?.display_name || id }, (c?.display_name || id).charAt(0).toUpperCase()); if (c?.color) { d.style.background = c.color; d.style.color = readableText(c.color); } wrap.append(d); }
  return wrap;
}
export function issueBadge(issues) {
  if (!issues?.length) return null;
  const worst = issues.some((i) => i.severity === 'error') ? 'bad' : 'warn';
  return badge(t('lib.issues', { count: issues.length }), worst, issues.map((i) => i.message).join('\n'));
}
export const sourceBadge = (src) => (src && src !== 'local' ? badge(src.replace(':', ' · '), 'info', t('lib.source_tip')) : null);

// Ask for kind + name, validate the id client-side (same rule as the server), then hand back {kind, name}.
export function newItemDialog({ kinds = KINDS.filter((k) => k !== 'rule' || true), kind = 'skill' } = {}) {
  return new Promise((resolve) => {
    let done = false;
    const sel = h('select', { id: 'ni-kind' }, kinds.map((k) => h('option', { value: k, selected: k === kind }, t(`kind.${k}`))));
    const title = h('input', { type: 'text', id: 'ni-title', autocomplete: 'off', placeholder: t('new.title_ph') });
    const name = h('input', { type: 'text', id: 'ni-name', autocomplete: 'off', spellcheck: 'false', placeholder: 'my-new-skill', 'aria-describedby': 'ni-err' });
    const err = h('div', { class: 'perr', id: 'ni-err', role: 'alert' });
    let touched = false;
    name.addEventListener('input', () => { touched = true; check(); });
    title.addEventListener('input', () => { if (!touched) name.value = slug(title.value); check(); });
    const check = () => { const ok = !name.value || validName(name.value); name.setAttribute('aria-invalid', String(!ok)); err.textContent = ok ? '' : t('new.bad_name'); return ok && !!name.value; };
    const form = h('form', { onSubmit: (e) => { e.preventDefault(); if (!check()) { name.focus(); return; } done = true; resolve({ kind: sel.value, name: name.value, title: title.value }); m.close(); } },
      field(t('new.kind'), sel), field(t('new.title'), title), field(t('new.name'), name, t('new.name_hint')), err,
      h('div', { class: 'row gap end' }, h('button', { type: 'button', class: 'btn', onClick: () => m.close() }, t('common.cancel')), h('button', { type: 'submit', class: 'btn primary' }, icon('plus', 16), h('span', t('new.create')))));
    const m = openModal(t('new.dialog_title'), form, { onClose: () => { if (!done) resolve(null); } });
    title.focus();
  });
}
