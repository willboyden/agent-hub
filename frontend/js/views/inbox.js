// Memory inbox: agents propose memories, only a human promotes them into content/memory/. Text is untrusted and shown as plain text.
import { AhView } from '../components/base.js';
import { h, clear } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, emptyBox, field, select, notifyChanged, timeText } from '../components/ui.js';
import { confirmDialog } from '../components/dialog.js';
import { flagSuspicious, filterInbox } from '../inbox.js';
import { timeAgo } from '../util.js';

class AhInbox extends AhView {
  setup() {
    this.client = '';
    this.host = h('div'); this.filter = h('div');
    this.append(h('div', { class: 'page-head' }, h('h1', t('inbox.title'))), h('p', { class: 'muted' }, t('inbox.intro')), this.filter, this.host);
    this.load();
  }
  async load() {
    await this.loadInto(this.host, async () => { this.items = await this.api.inbox(); this.paintFilter(); return this.cards(); });
  }
  paintFilter() {
    const clients = [...new Set(this.items.map((i) => i.client))];
    clear(this.filter).append(clients.length > 1 ? h('div', { class: 'row gap wrap', role: 'group', 'aria-label': t('inbox.filter') }, [['', t('inbox.all')], ...clients.map((c) => [c, c])].map(([v, l]) => h('button', { type: 'button', class: 'kchip', 'aria-pressed': String(this.client === v), onClick: () => { this.client = v; this.paintFilter(); clear(this.host).append(this.cards()); } }, l))) : '');
  }
  cards() {
    const list = filterInbox(this.items, this.client);
    if (!list.length) return emptyBox(t('inbox.empty_title'), t('inbox.empty_hint'));
    return h('div', { class: 'grid tiles inbox-grid' }, list.map((it) => this.card(it)));
  }
  card(it) {
    const flag = flagSuspicious(`${it.title}\n${it.body}`);
    const title = h('input', { type: 'text', id: `it-${it.id}`, value: it.title });
    const body = h('textarea', { rows: 5, id: `ib-${it.id}` }); body.value = it.body;
    const type = select(['user', 'feedback', 'project', 'reference'].map((x) => ({ value: x, label: t(`memtype.${x}`) })), it.type, null, { id: `ity-${it.id}` });
    const promote = btn(t('inbox.promote'), { icon: 'check', kind: 'primary', onClick: async () => {
      if (flag.suspicious && !(await confirmDialog(t('inbox.promote_suspicious'), { confirmLabel: t('inbox.promote') }))) return;
      try { const r = await this.api.promote(it.id, { title: title.value, body: body.value, type: type.value }); toast(t('inbox.promoted', { id: r.id }), { kind: 'ok' }); notifyChanged(); this.load(); } catch (e) { toastError(e); } } });
    return h('article', { class: `card inbox-card${flag.suspicious ? ' flagged' : ''}`, 'aria-label': it.title },
      h('header', { class: 'card-head' }, h('div', { class: 'row gap wrap' }, badge(it.client, 'info'), badge(t(`memtype.${it.type}`), 'muted')), h('span', { class: 'muted small' }, timeText(timeAgo(it.created_at)))),
      h('div', { class: 'card-body' },
        flag.suspicious ? h('p', { class: 'note bad', role: 'alert' }, h('strong', t('inbox.suspicious')), ' ', t('inbox.suspicious_hint', { reasons: flag.reasons.map((r) => t(`inbox.reason_${r}`)).join(', ') })) : null,
        field(t('inbox.f_title'), title), field(t('inbox.f_body'), body), field(t('inbox.type'), type),
        h('div', { class: 'row gap wrap' }, promote, btn(t('inbox.reject'), { icon: 'close', onClick: async () => { try { await this.api.reject(it.id); toast(t('inbox.rejected'), { kind: 'ok' }); notifyChanged(); this.load(); } catch (e) { toastError(e); } } }))));
  }
}
customElements.define('ah-inbox', AhInbox);
