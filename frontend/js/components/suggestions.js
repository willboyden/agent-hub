// "Suggested collections": the hub proposes groups for uncollected items. Each card shows why (reason chip), a preview of members with
// per-member removal, inline rename, and Accept / Dismiss. Dismissals are remembered in localStorage (ids only).
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { api } from '../app-context.js';
import { toast, toastError } from './toast.js';
import { btn, badge, notifyChanged } from './ui.js';
import { colorDot } from './parts.js';
import { loadDismissed, saveDismissed, buildOverrides, visibleSuggestions } from '../suggest-logic.js';

// onAccepted(ids) lets the page reload and flash the new collections.
export async function suggestionsPanel({ onAccepted } = {}) {
  let list;
  try { list = await api.suggestions(); } catch (e) { if (e.status === 404 || e.status === 405) return null; throw e; }
  const dismissed = loadDismissed(); const edits = {};
  const host = h('section', { class: 'card sugg-panel', 'aria-label': t('sugg.title') });
  const paint = () => {
    const shown = visibleSuggestions(list, dismissed);
    clear(host);
    if (!shown.length) { host.hidden = true; return; }
    host.hidden = false;
    const accept = async (ids) => {
      try {
        host.querySelectorAll('.sugg-card').forEach((c) => { if (ids.includes(c.dataset.id)) c.classList.add('accepting'); });
        await api.acceptSuggestions(ids, buildOverrides(edits, list.filter((s) => ids.includes(s.id))));
        toast(t('sugg.accepted', { count: ids.length }), { kind: 'ok' });
        list = list.filter((s) => !ids.includes(s.id)); notifyChanged(); onAccepted?.(ids); paint();
      } catch (e) { toastError(e); paint(); }
    };
    host.append(h('header', { class: 'card-head' }, h('div', h('h2', icon('star', 16), ' ', t('sugg.title')), h('p', { class: 'hint' }, t('sugg.intro', { count: shown.length }))),
      h('div', { class: 'row gap wrap' }, btn(t('sugg.accept_all'), { kind: 'primary', icon: 'check', onClick: () => accept(shown.map((s) => s.id)) }),
        btn(t('sugg.dismiss_all'), { onClick: () => { shown.forEach((s) => dismissed.add(s.id)); saveDismissed(dismissed); paint(); } }))),
      h('div', { class: 'card-body sugg-grid' }, shown.map((s) => card(s, accept))));
  };
  const card = (s, accept) => {
    const e = (edits[s.id] ||= { title: s.title, removed: new Set() });
    const members = s.members.filter((m) => !e.removed.has(`${m.kind}/${m.name}`));
    const title = h('input', { type: 'text', value: e.title, 'aria-label': t('sugg.rename', { name: s.title }), onInput: (ev) => { e.title = ev.target.value; } });
    const list_ = h('ul', { class: 'sugg-members' }, members.slice(0, 6).map((m) => h('li', h('span', { class: 'ellipsis' }, m.name), btn('', { icon: 'close', kind: 'ghost', size: 'sm', title: t('sugg.remove_member', { name: m.name }), onClick: () => { e.removed.add(`${m.kind}/${m.name}`); paint(); } }))),
      members.length > 6 ? h('li', { class: 'muted small' }, t('sugg.more', { count: members.length - 6 })) : null);
    return h('article', { class: 'sugg-card', dataset: { id: s.id }, 'aria-label': s.title },
      h('div', { class: 'row gap between wrap' }, h('div', { class: 'row gap' }, colorDot(s.color, 'swatch lg'), icon(s.icon, 18)), badge(t(`sugg.reason_${s.reason}`), 'info', t('sugg.reason_tip'))),
      title, s.description ? h('p', { class: 'muted small' }, s.description) : null, h('p', { class: 'small' }, t('sugg.members', { count: members.length })), list_,
      h('div', { class: 'row gap wrap' }, btn(t('sugg.accept'), { icon: 'check', kind: 'primary', size: 'sm', disabled: !members.length, onClick: () => accept([s.id]) }), btn(t('sugg.dismiss'), { size: 'sm', onClick: () => { dismissed.add(s.id); saveDismissed(dismissed); paint(); } })));
  };
  paint();
  return host;
}
