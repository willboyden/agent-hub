// Ctrl/Cmd-K command palette (ARIA combobox + listbox in a native <dialog>).
import { h } from '../dom.js';
import { t } from '../i18n.js';
import { fuzzyFilter } from '../fuzzy.js';

let providers = [];
export const registerCommands = (fn) => { providers.push(fn); return () => { providers = providers.filter((p) => p !== fn); }; };

class AhPalette extends HTMLElement {
  connectedCallback() {
    this.dlg = h('dialog', { class: 'palette', 'aria-label': t('palette.title') });
    this.input = h('input', { type: 'text', role: 'combobox', 'aria-expanded': 'true', 'aria-controls': 'pal-list', 'aria-autocomplete': 'list',
      placeholder: t('palette.placeholder'), autocomplete: 'off', spellcheck: 'false' });
    this.list = h('ul', { id: 'pal-list', role: 'listbox', class: 'pal-list' });
    this.dlg.append(this.input, this.list);
    this.append(this.dlg);
    this.input.addEventListener('input', () => { this.sel = 0; this.paint(); });
    this.dlg.addEventListener('keydown', (e) => this.onKey(e));
    this.dlg.addEventListener('click', (e) => { if (e.target === this.dlg) this.dlg.close(); });
    this._kd = (e) => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); this.open(); } };
    window.addEventListener('keydown', this._kd);
    this._open = () => this.open();
    window.addEventListener('ah:palette', this._open);
  }
  disconnectedCallback() { window.removeEventListener('keydown', this._kd); window.removeEventListener('ah:palette', this._open); }
  async open() {
    if (this.dlg.open) return;
    this.commands = (await Promise.all(providers.map((p) => Promise.resolve(p()).catch(() => [])))).flat();
    this.input.value = ''; this.sel = 0; this.paint();
    this.dlg.showModal(); this.input.focus();
  }
  paint() {
    this.shown = fuzzyFilter(this.input.value, this.commands || []).slice(0, 12);
    this.list.replaceChildren(...(this.shown.length ? this.shown.map((c, i) => h('li', { id: `pal-${i}`, role: 'option', class: i === this.sel ? 'sel' : '', 'aria-selected': String(i === this.sel),
      onClick: () => this.run(c), onMouseenter: () => { this.sel = i; this.mark(); } }, h('span', c.label), c.hint ? h('small', c.hint) : null))
      : [h('li', { class: 'none', role: 'presentation' }, t('palette.none'))]));
    this.input.setAttribute('aria-activedescendant', this.shown.length ? `pal-${this.sel}` : '');
  }
  mark() { [...this.list.children].forEach((li, i) => { li.classList.toggle('sel', i === this.sel); li.setAttribute('aria-selected', String(i === this.sel)); }); this.input.setAttribute('aria-activedescendant', `pal-${this.sel}`); }
  onKey(e) {
    if (e.key === 'ArrowDown') { e.preventDefault(); this.sel = Math.min(this.shown.length - 1, this.sel + 1); this.mark(); this.list.children[this.sel]?.scrollIntoView({ block: 'nearest' }); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); this.sel = Math.max(0, this.sel - 1); this.mark(); this.list.children[this.sel]?.scrollIntoView({ block: 'nearest' }); }
    else if (e.key === 'Enter') { e.preventDefault(); const c = this.shown[this.sel]; if (c) this.run(c); }
  }
  run(c) { this.dlg.close(); c.run(); }
}
customElements.define('ah-palette', AhPalette);
