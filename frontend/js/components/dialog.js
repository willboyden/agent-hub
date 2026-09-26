// Native <dialog> gives us modality, focus trapping and Esc-to-close for free (good a11y baseline).
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';

function make(cls, title, body, { onClose } = {}) {
  const opener = document.activeElement;
  const dlg = h('dialog', { class: cls, 'aria-labelledby': 'dlg-title' });
  const head = h('header', { class: 'dlg-head' }, h('h2', { id: 'dlg-title' }, title),
    h('button', { type: 'button', class: 'btn ghost sm', 'aria-label': t('common.close'), onClick: () => dlg.close() }, icon('close', 16)));
  const content = h('div', { class: 'dlg-body' }, body);
  dlg.append(head, content);
  dlg.addEventListener('close', () => { dlg.remove(); onClose?.(); opener?.focus?.(); });
  dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });
  document.body.append(dlg);
  dlg.showModal();
  return { el: dlg, body: content, close: () => dlg.close(), setTitle: (s) => { head.firstChild.textContent = s; } };
}
export const openDrawer = (title, body, o) => make('drawer', title, body, o);
export const openModal = (title, body, o) => make('modal', title, body, o);

export function confirmDialog(message, { confirmLabel = t('common.confirm'), danger = false } = {}) {
  return new Promise((resolve) => {
    let result = false;
    const m = openModal(t('common.confirm'), h('div', h('p', message),
      h('div', { class: 'row gap end' },
        h('button', { type: 'button', class: 'btn', onClick: () => m.close() }, t('common.cancel')),
        h('button', { type: 'button', class: `btn ${danger ? 'danger' : 'primary'}`, onClick: () => { result = true; m.close(); } }, confirmLabel))),
    { onClose: () => resolve(result) });
  });
}
