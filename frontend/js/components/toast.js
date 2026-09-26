import { h, icon } from '../dom.js';
import { describeError } from '../errors.js';
import { t } from '../i18n.js';
export function toast(message, { kind = 'info', timeout = 4500, action } = {}) {
  window.dispatchEvent(new CustomEvent('ah:toast', { detail: { message, kind, timeout, action } }));
}
export const toastError = (e, prefix) => toast(`${prefix ? prefix + ': ' : ''}${describeError(e)}`, { kind: 'bad', timeout: 8000 });

class AhToasts extends HTMLElement {
  connectedCallback() {
    // role=status + aria-live so screen readers announce toasts without stealing focus.
    this.setAttribute('role', 'status'); this.setAttribute('aria-live', 'polite');
    this._on = (e) => this.push(e.detail);
    window.addEventListener('ah:toast', this._on);
  }
  disconnectedCallback() { window.removeEventListener('ah:toast', this._on); }
  push({ message, kind, timeout, action }) {
    const close = h('button', { type: 'button', class: 'btn ghost sm', 'aria-label': t('common.dismiss'), onClick: () => el.remove() }, icon('close', 14));
    const act = action ? h('button', { type: 'button', class: 'btn sm', onClick: () => { el.remove(); action.run(); } }, action.label) : null;
    const el = h('div', { class: `toast ${kind}` }, h('span', message), act, close);
    this.append(el);
    if (timeout) setTimeout(() => el.remove(), timeout);
  }
}
customElements.define('ah-toasts', AhToasts);
