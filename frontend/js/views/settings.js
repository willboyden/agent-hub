// Settings and audit: runtime limits (the few settings the backend lets you change), read-only info, API keys (admin / viewer / per-client
// tokens), the read-only policy floor, and the audit log. Secrets are shown once at creation and never round-tripped.
import { AhView } from '../components/base.js';
import { h, icon, clear, copyText } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, tabs, emptyBox, field, select, notifyChanged, timeText } from '../components/ui.js';
import { confirmDialog, openModal } from '../components/dialog.js';
import { isRedacted, listOf } from '../data.js';
import { floorRows, isFloorKey, groupFindings } from '../adapt.js';
import { timeAgo } from '../util.js';

const EDITABLE = { plan_retention: [1, 200], inbox_max_per_client: [1, 5000], inbox_rate_per_min: [1, 600] };

class AhSettings extends AhView {
  setup() {
    this.tab = this.query?.tab || 'general';
    this.panel = h('div', { role: 'tabpanel' });
    this.append(h('div', { class: 'page-head' }, h('h1', t('set.title'))), tabs(['general', 'doctor', 'keys', 'floor', 'audit'].map((id) => ({ id, label: t(`set.tab_${id}`) })), this.tab, (id) => { this.tab = id; this.paint(); }, t('editor.tabs')), this.panel);
    this.paint();
  }
  paint() { this.loadInto(this.panel, async () => this[`${this.tab}Tab`]()); }

  async generalTab() {
    const s = await this.api.settings();
    const info = s.info || {};
    const inputs = Object.fromEntries(Object.entries(s.editable).map(([k, v]) => [k, h('input', { type: 'number', id: `s-${k}`, value: v, min: EDITABLE[k]?.[0] ?? 1, max: EDITABLE[k]?.[1] ?? 100000, 'aria-describedby': `s-${k}-h` })]));
    const err = h('div', { class: 'perr', role: 'alert' });
    const facts = [['set.content_dir', info.content_dir], ['set.data_dir', info.data_dir ?? info.state_dir], ['set.policy_file', info.policy_file ?? info.floor_path], ['set.host', info.host != null ? `${info.host}:${info.port ?? ''}` : null],
      ['set.adapters', (info.adapters || []).join(', ')], ['set.trust_loopback', info.trust_loopback == null ? null : t(info.trust_loopback ? 'set.on' : 'set.off')]].filter(([, v]) => v);
    const kn = info.knowledge;
    return h('div', { class: 'stack-v' },
      info.trust_loopback ? h('p', { class: 'note warn', role: 'alert' }, t('set.trust_warn')) : null,
      h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('set.paths'))), h('div', { class: 'card-body' }, h('dl', { class: 'facts' }, facts.map(([k, v]) => h('div', { class: 'kv' }, h('dt', t(k)), h('dd', h('code', String(v)))))), h('p', { class: 'hint' }, t('set.paths_hint')),
        kn ? h('p', { class: 'small' }, h('strong', t('set.knowledge_status')), ' ', h('code', typeof kn === 'string' ? kn : JSON.stringify(kn))) : null,
        (info.adapter_load_errors && Object.keys(info.adapter_load_errors).length) ? h('p', { class: 'note bad' }, t('set.adapter_errors'), ' ', JSON.stringify(info.adapter_load_errors)) : null)),
      Object.keys(inputs).length ? h('form', { class: 'card', onSubmit: async (e) => { e.preventDefault(); err.textContent = '';
        const body = Object.fromEntries(Object.entries(inputs).map(([k, el]) => [k, Number(el.value)]));
        for (const [k, v] of Object.entries(body)) { const [lo, hi] = EDITABLE[k] || [1, 100000]; if (!Number.isInteger(v) || v < lo || v > hi) { err.textContent = t('set.range', { name: t(`set.f_${k}`), lo, hi }); return; } }
        try { await this.api.saveSettings(body); toast(t('set.saved'), { kind: 'ok' }); notifyChanged(); this.paint(); } catch (x) { err.textContent = x.message; toastError(x); } } },
      h('header', { class: 'card-head' }, h('h2', t('set.limits'))), h('div', { class: 'card-body' }, h('p', { class: 'hint' }, t('set.limits_hint')),
        ...Object.entries(inputs).map(([k, el]) => field(t(`set.f_${k}`), el, t(`set.f_${k}_hint`))), err, h('div', btn(t('common.save'), { icon: 'check', kind: 'primary', type: 'submit' })))) : null,
      h('p', { class: 'note info' }, t('set.endpoints_where')));
  }

  // Host hardening: what the hub's doctor found on this machine. Fixes are commands or JSON lines: shown as plain text with a copy button.
  async doctorTab() {
    const d = await this.api.doctor();
    const g = groupFindings(d.findings);
    const chip = { ok: 'ok', attention: 'warn', action_needed: 'bad' }[d.status] || 'muted';
    const card = (f) => h('article', { class: `card finding sev-${f.severity}` }, h('div', { class: 'card-body' },
      h('div', { class: 'row gap wrap' }, badge(t(`doc.sev_${f.severity}`), { crit: 'bad', warn: 'warn', info: 'info', ok: 'ok' }[f.severity]), h('strong', f.title)),
      f.detail ? h('p', { class: 'muted' }, f.detail) : null,
      f.fix ? h('div', { class: 'stack-v tight' }, h('span', { class: 'lbl' }, t('doc.fix')), h('pre', { class: 'cmd fix', tabindex: '0', 'aria-label': t('doc.fix') }, f.fix),
        h('div', btn(t('common.copy'), { icon: 'copy', size: 'sm', onClick: async () => toast((await copyText(f.fix)) ? t('common.copied') : t('common.copy_failed'), { kind: 'ok' }) }))) : null));
    const open = [...g.crit, ...g.warn, ...g.info];
    return h('div', { class: 'stack-v' },
      h('div', { class: 'row between wrap gap' }, h('div', { class: 'row gap wrap' }, badge(t(`doc.status_${d.status}`), chip), h('span', { class: 'muted small' }, t('doc.counts', { crit: g.crit.length, warn: g.warn.length, info: g.info.length }))),
        btn(t('doc.recheck'), { icon: 'restart', onClick: () => { toast(t('doc.rechecking'), { kind: 'info', timeout: 1500 }); this.paint(); } })),
      h('p', { class: 'muted' }, t('doc.intro')),
      open.length ? open.map(card) : h('p', { class: 'note ok' }, t('doc.all_ok')),
      g.ok.length ? h('details', { class: 'doc-ok' }, h('summary', t('doc.ok_n', { count: g.ok.length })), h('div', { class: 'stack-v' }, g.ok.map(card))) : null);
  }

  async keysTab() {
    const keys = listOf(await this.api.keys());
    const name = h('input', { type: 'text', id: 'key-name', autocomplete: 'off', placeholder: t('set.key_name_ph') });
    const role = select([{ value: 'viewer', label: t('set.role_viewer') }, { value: 'admin', label: t('set.role_admin') }, { value: 'client', label: t('set.role_client') }], 'viewer', (v) => { clientBox.hidden = v !== 'client'; }, { id: 'key-role' });
    const client = h('input', { type: 'text', id: 'key-client', autocomplete: 'off', placeholder: 'hermes' });
    const scopes = h('input', { type: 'text', id: 'key-scopes', autocomplete: 'off', placeholder: 'inbox:write, knowledge:read:team-docs' });
    const clientBox = h('div', { class: 'stack-v', hidden: true }, field(t('set.key_client'), client, t('set.key_client_hint')), field(t('set.key_scopes'), scopes, t('set.key_scopes_hint')));
    const err = h('div', { class: 'perr', role: 'alert' });
    return h('div', { class: 'stack-v' }, h('p', { class: 'muted' }, t('set.keys_intro')),
      h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['set.k_name', 'set.k_role', 'set.k_prefix', 'set.k_scopes', 'set.k_created', 'set.k_used', ''].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('common.delete')))))),
        h('tbody', keys.map((k) => h('tr', h('td', h('strong', k.name), k.client ? h('div', { class: 'muted small' }, k.client) : null), h('td', badge(t(`set.role_${k.role}`), k.role === 'admin' ? 'warn' : 'muted')), h('td', h('code', `${k.prefix}…`)),
          h('td', h('div', { class: 'row gap wrap' }, (k.scopes || []).map((sc) => badge(String(sc.ns ? `${sc.ns}: ${sc.mode}` : sc), 'muted')))),
          h('td', { class: 'muted nowrap' }, k.created_at ? timeText(timeAgo(k.created_at)) : ''), h('td', { class: 'muted nowrap' }, k.last_used_at ? timeText(timeAgo(k.last_used_at)) : t('common.never')),
          h('td', { class: 'row' }, btn('', { icon: 'trash', kind: 'ghost', size: 'sm', title: t('set.key_delete', { name: k.name }), onClick: async () => { if (await confirmDialog(t('set.key_delete_confirm', { name: k.name }), { danger: true, confirmLabel: t('common.delete') })) { try { await this.api.deleteKey(k.id); this.paint(); } catch (e) { toastError(e); } } } }))))))),
      h('form', { class: 'card', onSubmit: async (e) => { e.preventDefault(); err.textContent = ''; if (!name.value.trim()) { err.textContent = t('set.key_need_name'); return; }
        const body = { name: name.value.trim(), role: role.value };
        if (role.value === 'client') { if (!client.value.trim()) { err.textContent = t('set.key_need_client'); return; } body.client = client.value.trim(); body.scopes = scopes.value.split(',').map((x) => x.trim()).filter(Boolean); }
        try { const k = await this.api.createKey(body); this.secretDialog(k); this.paint(); } catch (x) { err.textContent = x.message; toastError(x); } } },
        h('header', { class: 'card-head' }, h('h2', t('set.key_create'))), h('div', { class: 'card-body' }, h('div', { class: 'form-grid' }, field(t('set.k_name'), name), field(t('set.k_role'), role)), clientBox, err, h('div', btn(t('set.key_create'), { icon: 'plus', kind: 'primary', type: 'submit' })))));
  }
  secretDialog(k) {
    const m = openModal(t('set.key_created', { name: k.name }), h('div', { class: 'stack-v' }, h('p', { class: 'note warn' }, t('k.secret_once')), h('code', { class: 'ro secret' }, k.secret),
      h('div', { class: 'row gap end' }, btn(t('common.copy'), { icon: 'copy', onClick: async () => toast((await copyText(k.secret)) ? t('common.copied') : t('common.copy_failed'), { kind: 'ok' }) }), btn(t('common.close'), { kind: 'primary', onClick: () => m.close() }))));
  }

  async floorTab() {
    const rows = floorRows(await this.api.floor());
    return h('div', { class: 'stack-v' }, h('p', { class: 'note info' }, h('span', { class: 'lock' }, icon('lock', 14)), ' ', t('set.floor_intro')),
      rows.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'compact floor-table' }, h('thead', h('tr', ['', 'rule.col_rule', 'rule.col_decision', 'rule.col_why'].map((k) => h('th', { scope: 'col' }, k ? t(k) : h('span', { class: 'sr-only' }, t('lib.locked')))))),
        h('tbody', rows.map((r) => h('tr', { class: 'locked-row' }, h('td', h('span', { class: 'lock' }, icon('lock', 16))), h('td', h('strong', isFloorKey(r.title) ? t(r.title) : r.title), h('div', { class: 'muted small' }, `${t(`rule.kind_${r.kind}`)}: `, h('code', r.match))),
          h('td', badge(t(`rule.dec_${r.decision}`), r.decision === 'deny' ? 'bad' : r.decision === 'ask' ? 'warn' : 'ok'), r.extra ? h('div', { class: 'muted small' }, t('floor.tool_default', { value: t(`rule.dec_${r.extra}`) })) : null), h('td', { class: 'small' }, isFloorKey(r.reason) ? t(r.reason) : r.reason)))))) : emptyBox(t('set.floor_none'), ''));
  }

  async auditTab() {
    this.cursor = null; this.all = [];
    const body = h('tbody'); const more = h('div', { class: 'row' }); const status = h('p', { class: 'muted small', role: 'status', 'aria-live': 'polite' });
    let q = '';
    const paintRows = () => {
      const shown = this.all.filter((e) => !q || `${e.action} ${e.target} ${e.detail} ${e.actor}`.toLowerCase().includes(q));
      body.replaceChildren(...shown.map((e) => h('tr', h('td', { class: 'nowrap muted' }, new Date(e.ts * 1000).toLocaleString()), h('td', e.actor ? `${e.actor}${e.role ? ` (${e.role})` : ''}` : ''), h('td', h('code', e.action)), h('td', { class: 'small audit-detail' }, e.detail || ''), h('td', e.ok ? badge(e.status ? String(e.status) : t('set.a_ok'), 'ok') : badge(e.status ? String(e.status) : t('set.a_fail'), 'bad')))));
      status.textContent = shown.length ? t('set.a_count', { count: shown.length }) : t('set.a_none');
    };
    const load = async () => {
      const page = await this.api.auditLog({ cursor: this.cursor, limit: 50 });
      this.all.push(...page.items); this.cursor = page.next_cursor; clear(more).append(this.cursor ? btn(t('set.a_more'), { onClick: () => load().catch(toastError) }) : ''); paintRows();
    };
    await load();
    const input = h('input', { type: 'search', id: 'audit-q', placeholder: t('set.a_filter'), 'aria-label': t('set.a_filter'), onInput: (e) => { q = e.target.value.toLowerCase(); paintRows(); } });
    return h('div', { class: 'stack-v' }, h('p', { class: 'hint' }, t('set.a_hint')), h('div', { class: 'row gap' }, h('div', { class: 'grow' }, input)), status,
      h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', ['set.a_when', 'set.a_actor', 'set.a_action', 'set.a_detail', 'set.a_result'].map((k) => h('th', { scope: 'col' }, t(k))))), body)), more);
  }
}
void isRedacted;
customElements.define('ah-settings', AhSettings);
