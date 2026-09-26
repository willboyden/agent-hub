// Home: first-run checklist (Import -> Organise -> Review plan -> Apply), what needs attention, and a summary of the library.
import { AhView } from '../components/base.js';
import { h, icon } from '../dom.js';
import { t } from '../i18n.js';
import { local } from '../store.js';
import { badge, clientStatusBadge, kindBadge, card, emptyBox } from '../components/ui.js';
import { KINDS } from '../api.js';
import { checklist, readPlanViewed } from '../checklist.js';

class AhHome extends AhView {
  setup() {
    this.host = h('div');
    this.append(h('div', { class: 'page-head' }, h('h1', t('home.title'))), h('p', { class: 'muted' }, t('home.intro')), this.host);
    this.loadInto(this.host, () => this.build(), { skeletonRows: 5 });
  }
  async build() {
    const [clients, collections, health, inbox, changes] = await Promise.all([this.api.clients(), this.api.collections(), this.api.health(), this.api.inbox(), this.api.changes()]);
    const pending = changes.count;
    const all = await Promise.all(KINDS.map((k) => this.api.items(k)));
    const totals = Object.fromEntries(KINDS.map((k, i) => [k, all[i].filter((x) => !x.locked).length]));
    const items = all.flat().filter((x) => !x.locked);
    const steps = checklist({ itemCount: items.length, collectionCount: collections.length, viewedHash: readPlanViewed(), contentHash: changes.content_hash, appliedCount: clients.filter((c) => c.last_applied_at).length });
    const done = steps.filter((s) => s.done).length;
    const attention = [];
    // Host hardening summary: only when the route exists and answers; failures are ignored quietly.
    const ds = await this.api.doctorSummary().catch(() => null);
    if (ds && (ds.crit || ds.warn)) attention.unshift({ href: '#/settings?tab=doctor', text: t('home.a_doctor', { crit: ds.crit, warn: ds.warn }), kind: ds.crit ? 'bad' : 'warn' });
    const issues = items.filter((i) => (i.issues || []).length);
    if (issues.length) attention.push({ href: '#/library?issues=1', text: t('home.a_issues', { count: issues.length }), kind: 'warn' });
    if (health.content_initialised === false) attention.push({ href: '#/settings', text: t('app.uninitialised'), kind: 'bad' });
    if (pending) attention.push({ href: '#/changes', text: t('home.a_pending', { count: pending }), kind: 'info' });
    const inboxN = inbox.length; if (inboxN) attention.push({ href: '#/inbox', text: t('home.a_inbox', { count: inboxN }), kind: 'info' });
    for (const c of clients) {
      const href = `#/clients/${encodeURIComponent(c.id)}`;
      if (c.status === 'edited_outside') attention.push({ href, text: t('home.a_drift', { name: c.display_name }), kind: 'warn' });
      else if (c.status === 'pending_changes') attention.push({ href, text: t('home.a_pending_files', { name: c.display_name, count: c.drift.pending_changes }), kind: 'info' });
      else if (c.status === 'error') attention.push({ href, text: t('home.a_blocked', { name: c.display_name }), kind: 'bad' });
    }
    return h('div', { class: 'stack-v' },
      h('section', { class: 'card checklist-card' }, h('header', { class: 'card-head' }, h('h2', t('home.checklist')), h('span', { class: 'muted' }, t('home.progress', { done, total: steps.length }))),
        h('ol', { class: 'checklist' }, steps.map((s, i) => h('li', { class: `${s.done ? 'done' : ''}${s.next ? ' next' : ''}` },
          h('span', { class: 'dot', 'aria-hidden': 'true' }, s.done ? '✓' : String(i + 1)), h('div', { class: 'cl-text' }, h('strong', t(`home.step_${s.id}`)), h('div', { class: 'muted small' }, t(`home.step_${s.id}_hint`)), s.done ? h('span', { class: 'sr-only' }, t('home.done')) : null),
          h('a', { class: `btn ${s.next ? 'primary' : ''}`.trim(), href: s.href }, t(`home.step_${s.id}_cta`)))))),
      h('div', { class: 'grid two' },
        card(t('home.attention'), attention.length ? h('ul', { class: 'attn' }, attention.map((a) => h('li', h('a', { href: a.href }, badge(a.text, a.kind))))) : h('p', { class: 'note ok' }, t('home.all_good'))),
        card(t('home.library'), h('div', { class: 'grid stat-grid' }, KINDS.map((k) => h('a', { class: 'stat', href: `#/library?kind=${k}` }, h('span', { class: 'big num' }, String(totals[k])), h('span', { class: 'muted small' }, t(`kind.${k}s`))))))),
      card(t('home.clients'), clients.length ? h('div', { class: 'grid tiles' }, clients.map((c) => h('a', { class: 'mini-client', href: `#/clients/${encodeURIComponent(c.id)}` }, h('strong', c.display_name), clientStatusBadge(c)))) : emptyBox(t('cl.empty_title'), t('cl.empty_hint'), h('a', { class: 'btn primary', href: '#/clients' }, t('cl.add')))));
  }
}
void icon; void kindBadge;
customElements.define('ah-home', AhHome);
