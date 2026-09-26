// "Enabled for" card: per-client switches for one item, driven by GET /matrix cells. Blocked cells explain why; unsupported are greyed.
import { h } from '../dom.js';
import { t } from '../i18n.js';
import { api } from '../app-context.js';
import { toast, toastError } from './toast.js';
import { badge, notifyChanged } from './ui.js';

export async function enabledCard(kind, name) {
  const m = await api.matrix();
  const row = m.groups.flatMap((g) => g.rows).find((r) => r.kind === kind && r.name === name);
  if (!row) return h('p', { class: 'muted' }, t('editor.enable_na'));
  const body = h('div', { class: 'enable-table' });
  const paint = (r) => body.replaceChildren(...m.columns.map((c) => {
    const cell = r.cells[c.id] || { state: 'unsupported' };
    const on = cell.state === 'on' || cell.state === 'via_collection';
    const off = cell.state === 'unsupported' || (cell.state === 'blocked' && !on);
    const id = `en-${c.id}`;
    const input = h('input', { type: 'checkbox', id, role: 'switch', checked: on, disabled: off, 'aria-describedby': `${id}-why`,
      onChange: async (e) => {
        try { await api.matrixToggle({ client: c.id, kind, name, enabled: e.target.checked }); const fresh = (await api.matrix()).groups.flatMap((g) => g.rows).find((x) => x.kind === kind && x.name === name); if (fresh) { r = fresh; paint(r); } notifyChanged(); toast(t(e.target.checked ? 'editor.enabled_for' : 'editor.disabled_for', { client: c.display_name }), { kind: 'ok' }); }
        catch (err) { e.target.checked = !e.target.checked; toastError(err); }
      } });
    const why = cell.state === 'blocked' ? cell.reason : cell.state === 'unsupported' ? cell.reason : cell.state === 'via_collection' ? t('mx.via_hint') : '';
    return h('div', { class: `enable-row-x ${cell.state}` }, input, h('label', { for: id }, c.display_name),
      cell.state === 'blocked' ? badge(t('mx.state_blocked'), 'bad') : cell.state === 'unsupported' ? badge(t('mx.state_unsupported'), 'muted') : cell.state === 'via_collection' ? badge(t('mx.state_via_collection'), 'info') : null,
      h('span', { class: 'muted small', id: `${id}-why` }, why));
  }));
  paint(row);
  return body;
}
