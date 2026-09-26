// "Move to collection..." picker: a keyboard- and screen-reader-friendly alternative to drag-and-drop.
// A dialog with a real radio list (or checkbox list for multi-membership), so it needs no pointer at all.
import { h } from '../dom.js';
import { t } from '../i18n.js';
import { openModal } from './dialog.js';

// collections: [{id,title,color}], current: Set of ids the item(s) already belong to.
export function pickCollections({ title, collections, current = new Set(), multi = true, confirmLabel }) {
  return new Promise((resolve) => {
    let done = false;
    const boxes = collections.map((c) => h('label', { class: 'pick-row' }, h('input', { type: multi ? 'checkbox' : 'radio', name: 'pick', value: c.id, checked: current.has(c.id) }),
      h('i', { class: 'swatch', dataset: { color: c.color } }), h('span', c.title)));
    for (const b of boxes) { const sw = b.querySelector('.swatch'); sw.style.background = sw.dataset.color; }
    const form = h('form', { onSubmit: (e) => { e.preventDefault(); done = true; resolve([...form.querySelectorAll('input:checked')].map((i) => i.value)); m.close(); } },
      h('p', { class: 'hint' }, multi ? t('picker.hint_multi') : t('picker.hint_one')),
      h('div', { class: 'pick-list' }, collections.length ? boxes : h('p', { class: 'muted' }, t('picker.none'))),
      h('div', { class: 'row gap end' }, h('button', { type: 'button', class: 'btn', onClick: () => m.close() }, t('common.cancel')), h('button', { type: 'submit', class: 'btn primary', disabled: !collections.length }, confirmLabel || t('common.save'))));
    const m = openModal(title, form, { onClose: () => { if (!done) resolve(null); } });
    form.querySelector('input')?.focus();
  });
}
