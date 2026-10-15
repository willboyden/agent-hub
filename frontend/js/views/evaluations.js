// Evaluations (F11): every skill the hub holds or a client has been seen with, evaluated with SkillEvaluator. A status line, summary chips,
// filters, a dense multi-select table, a FIFO queue bar (polled every 2 s only while it is non-empty), a detail drawer and a bulk
// "Import into hub..." for observed skills. Reads never start a process; only the buttons enqueue work.
import { AhView } from '../components/base.js';
import { h, icon, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { local } from '../store.js';
import { toast, toastError } from '../components/toast.js';
import { btn, emptyBox, field, notifyChanged, timeText } from '../components/ui.js';
import { openDrawer, confirmDialog } from '../components/dialog.js';
import { gateBadge, verdictBadge, originBadge, hubStatusBadge, dash } from '../components/eval-parts.js';
import { detailBody } from '../components/eval-detail.js';
import { openImportDialog } from '../components/eval-import.js';
import { applySelection, pruneSelection, selectAll } from '../select.js';
import { debounce, timeAgo } from '../util.js';
import { tierKey } from '../skilleval.js';
import { ORIGINS, STATES, GATES, SORTS, EMPTY_EV_FILTERS, filtersFromQuery, filtersToQuery, activeEvFilterCount, filterEvalRows, summarizeRows, summaryChips,
  sortEvalRows, rowNumbers, offeredTiers, runBody, runOutcome, isObserved, importableRows, clientsOf, queueProgress, pollInit, pollStep, splitSubject } from '../evalview.js';
import { VERDICTS } from '../skilleval.js';

class AhEvaluations extends AhView {
  setup() {
    this.f = filtersFromQuery(this.query);
    this.sort = local.get('ah.ev.sort', 'name');
    this.tiers = local.get('ah.ev.tiers', 'static');
    this.sel = { ids: new Set(), anchor: null };
    this.rows = []; this.clients = []; this.status = null; this.gateMode = null; this.poll = pollInit(); this.timer = null; this.queue = null;
    this.head = h('div', { class: 'row between wrap gap page-head' }, h('h1', t('ev.title')));
    this.statusLine = h('p', { class: 'muted ev-status', role: 'status' });
    this.chips = h('div', { class: 'ev-chips', role: 'group', 'aria-label': t('ev.chips_label') });
    this.toolbar = h('div', { class: 'ev-toolbar' });
    this.actions = h('div', { class: 'ev-actions-bar row gap wrap' });
    this.queueBar = h('div', { class: 'ev-queue', role: 'status', 'aria-live': 'polite', hidden: true });
    this.list = h('div', { class: 'ev-list' });
    this.live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.append(this.head, this.statusLine, this.queueBar, this.chips, this.toolbar, this.actions, this.list, this.live);
    this.own(() => this.stopPolling());
    this.listen(this, 'keydown', (e) => { if (e.key === 'Escape' && this.sel.ids.size && !document.querySelector('dialog[open]')) { this.sel = { ids: new Set(), anchor: null }; this.paint(); } });
    this.init();
  }
  async init() {
    this.loadInto(this.list, async () => { await this.fetchAll(); return null; }, { skeletonRows: 8 }).then(() => { if (this._alive && !this.failed) { this.buildToolbar(); this.paint(); this.checkQueue(); } });
  }
  async fetchAll() {
    this.failed = true;
    const [clients, status, settings] = await Promise.all([this.api.clients(), this.api.skillEvaluator().catch(() => null), this.api.settings().catch(() => null)]);
    this.clients = clients; this.status = status; this.gateMode = settings?.editable?.eval_gate ?? null; this.gateObserved = settings?.editable?.eval_gate_observed ?? this.gateMode; this.minTiers = settings?.editable?.eval_gate_min_tiers ?? null;
    if (!offeredTiers(status).includes(this.tiers)) this.tiers = offeredTiers(status)[0];
    await this.loadRows();
    this.failed = false;
  }
  async loadRows() {
    const r = await this.api.skillEvaluations({ source: this.f.source });
    this.rows = r.items;
    this.rowById = new Map(this.rows.map((x) => [x.subject, x]));
  }
  async reload(quiet = true) {
    try { await this.loadRows(); this.paint(); } catch (e) { if (!quiet) toastError(e); }
  }

  // ---- queue polling (state machine in evalview.js; this only does the I/O) ----
  async checkQueue() { try { this.applyQueue(await this.api.evalQueue()); } catch { /* a missing queue route just means no bar */ } }
  stopPolling() { clearTimeout(this.timer); this.timer = null; }
  step(ev) {
    const r = pollStep(this.poll, ev); this.poll = r.state; this.stopPolling();
    if (r.refresh) this.reload(true).then(() => this.drawerReload?.());
    if (r.delay != null && this._alive) this.timer = setTimeout(() => this.tick(), r.delay);
  }
  async tick() {
    if (!this._alive) return;
    try { const q = await this.api.evalQueue(); if (!this._alive) return; this.applyQueue(q, true); } catch (e) { if (this._alive) this.step({ type: 'error', error: e }); }
  }
  applyQueue(q, polling = false) { this.queue = q; this.paintQueue(); this.step({ type: 'queue', queue: q }); void polling; }
  paintQueue() {
    const p = queueProgress(this.queue);
    this.queueBar.hidden = !p;
    if (!p) { clear(this.queueBar); return; }
    clear(this.queueBar).append(icon('restart', 16), h('span', { class: 'ev-queue-text' }, p.running ? h('span', t('ev.q_running'), ' ', h('code', p.running)) : h('span', t('ev.q_starting')), ' · ', t('ev.q_waiting', { count: p.waiting }), ' · ', t('ev.q_done', { count: p.done }), p.errors ? ` · ${t('ev.q_errors', { count: p.errors })}` : ''),
      btn(t('ev.q_cancel'), { icon: 'close', size: 'sm', onClick: () => this.cancel() }));
  }
  async cancel() {
    try { await this.api.cancelEvalQueue(); toast(t('ev.q_cancelled'), { kind: 'warn' }); this.step({ type: 'stop' }); this.queue = null; this.paintQueue(); await this.reload(); } catch (e) { toastError(e); }
  }
  async run(body) {
    try {
      const o = runOutcome(await this.api.runEvaluations(body));
      toast(t('ev.queued_toast', { queued: o.queued, skipped: o.skipped }), { kind: o.queued ? 'ok' : 'warn' });
      this.live.textContent = t('ev.queued_toast', { queued: o.queued, skipped: o.skipped });
      this.step({ type: 'start' }); this.sel = { ids: new Set(), anchor: null };
      await this.reload();
    } catch (e) { toastError(e); }
  }

  // ---- toolbar ----
  setFilter(k, v) { this.f[k] = v; if (k === 'source') { this.sel = { ids: new Set(), anchor: null }; this.syncUrl(); this.loadInto(this.list, async () => { await this.loadRows(); return null; }).then(() => { this.buildToolbar(); this.paint(); }); return; } this.syncUrl(); this.paint(); this.syncChips(); }
  syncUrl() { const q = new URLSearchParams(filtersToQuery(this.f)).toString(); history.replaceState(null, '', `#/evaluations${q ? `?${q}` : ''}`); }
  buildToolbar() {
    const sel = (id, label, opts, key) => field(label, h('select', { id, onChange: (e) => this.setFilter(key, e.target.value) },
      [h('option', { value: '' }, t('ev.any')), ...opts.map((o) => h('option', { value: o.value, selected: this.f[key] === o.value }, o.label))]));
    const sources = [{ value: 'hub', label: t('ev.source_hub') }, { value: 'all', label: t('ev.source_all') }, ...this.clients.map((c) => ({ value: c.id, label: c.display_name }))];
    const source = field(t('ev.f_source'), h('select', { id: 'ev-source', onChange: (e) => this.setFilter('source', e.target.value) }, sources.map((o) => h('option', { value: o.value, selected: this.f.source === o.value }, o.label))));
    const q = h('input', { type: 'search', id: 'ev-q', value: this.f.q, placeholder: t('ev.search_ph'), autocomplete: 'off', onInput: debounce((e) => this.setFilter('q', e.target.value), 120) });
    const sortSel = field(t('ev.sort'), h('select', { id: 'ev-sort', onChange: (e) => { this.sort = e.target.value; local.set('ah.ev.sort', this.sort); this.paint(); } }, SORTS.map((s) => h('option', { value: s, selected: s === this.sort }, t(`ev.sort_${s}`)))));
    clear(this.toolbar).append(h('div', { class: 'filters ev-filters' }, source, field(t('ev.f_search'), q),
      sel('ev-origin', t('ev.f_origin'), ORIGINS.map((o) => ({ value: o, label: t(`ev.origin_${o}`) })), 'origin'),
      sel('ev-state', t('ev.f_state'), STATES.map((s) => ({ value: s, label: t(`ev.state_${s}`) })), 'state'),
      sel('ev-verdict', t('ev.f_verdict'), VERDICTS.map((v) => ({ value: v, label: t(`ev.verdict_${v}`) })), 'verdict'),
      sel('ev-gate', t('ev.f_gate'), GATES.map((g) => ({ value: g, label: t(`ev.gate_${g}`) })), 'gate'), sortSel));
  }
  paintStatus() {
    const s = this.status;
    const gate = this.gateMode ? ` · ${t('ev.status_gate', { mode: t(`ev.mode_${this.gateMode}`), observed: t(`ev.mode_${this.gateObserved || this.gateMode}`), tiers: this.minTiers ? t(`ev.tier_${tierKey(this.minTiers)}`) : '-' })}` : '';
    clear(this.statusLine).append(!s ? t('ev.status_unknown') : s.installed
      ? h('span', icon('check', 14), ' ', t('ev.status_ok', { launcher: s.launcher || '-', version: s.version || '?', tiers: offeredTiers(s).map((x) => t(`ev.tier_${tierKey(x)}`)).join(', ') }), gate)
      : h('span', { class: 'note warn' }, t('ev.status_missing'), ' ', s.install_hint ? h('code', s.install_hint) : null));
  }
  tierPicker(id = 'ev-tiers', value = this.tiers, onChange) {
    const ids = offeredTiers(this.status);
    return h('div', { class: 'ev-tier' }, h('label', { for: id, class: 'lbl' }, t('ev.tier_label')),
      h('select', { id, 'aria-describedby': `${id}-h`, onChange: (e) => { this.tiers = e.target.value; local.set('ah.ev.tiers', this.tiers); onChange?.(this.tiers); } },
        ids.map((x) => h('option', { value: x, selected: x === value }, t(`ev.tier_${tierKey(x)}`)))),
      h('span', { class: 'sr-only', id: `${id}-h` }, t('ev.tier_hint')));
  }

  // ---- table ----
  visible() { return sortEvalRows(filterEvalRows(this.rows, this.f), this.sort); }
  syncChips() {
    const s = summarizeRows(this.rows);
    clear(this.chips).append(h('span', { class: 'ev-total' }, t('ev.total', { count: s.total })),
      ...summaryChips(s).map((c) => h('button', { type: 'button', class: 'kchip ev-chip', 'aria-pressed': String(this.f[c.filter] === c.value), onClick: () => { this.f[c.filter] = this.f[c.filter] === c.value ? '' : c.value; this.syncUrl(); this.buildToolbar(); this.paint(); } },
        t(c.key), h('span', { class: 'tab-count' }, String(c.count)))));
  }
  paint() {
    this.paintStatus(); this.syncChips();
    const vis = this.vis = this.visible(); this.order = vis.map((r) => r.subject);
    this.sel = pruneSelection(this.sel, this.rows.map((r) => r.subject));
    this.live.textContent = t('ev.result_count', { count: vis.length, total: this.rows.length });
    this.paintActions();
    if (!this.rows.length) {
      const isClient = !['hub', 'all'].includes(this.f.source);
      clear(this.list).append(isClient ? emptyBox(t('ev.empty_client_title'), t('ev.empty_client_hint'), btn(t('ev.observe_now'), { icon: 'restart', kind: 'primary', onClick: () => this.observe(this.f.source) })) : emptyBox(t('ev.empty_title'), t('ev.empty_hint')));
      return;
    }
    if (!vis.length) { clear(this.list).append(emptyBox(t('ev.none_title'), t('ev.none_hint'), btn(t('lib.clear_filters'), { onClick: () => { this.f = { ...EMPTY_EV_FILTERS, source: this.f.source }; this.syncUrl(); this.buildToolbar(); this.paint(); } }))); return; }
    const nf = activeEvFilterCount(this.f);
    const allOn = this.sel.ids.size === vis.length && vis.length > 0;
    const showWhere = this.f.source === 'all';
    clear(this.list).append(h('div', { class: 'row between wrap gap lib-meta' },
      h('span', { class: 'muted small' }, t('ev.result_count', { count: vis.length, total: this.rows.length }), nf ? ` · ${t('lib.filters_active', { count: nf })}` : ''),
      h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: allOn, indeterminate: this.sel.ids.size > 0 && !allOn, onChange: (e) => { this.sel = e.target.checked ? selectAll(this.order) : { ids: new Set(), anchor: null }; this.paint(); } }), h('span', t('ev.select_all')))),
      h('div', { class: 'tablewrap' }, h('table', { class: 'compact ev-table' }, h('caption', { class: 'sr-only' }, t('ev.title')),
        h('thead', h('tr', [['', 'ev.select'], ['ev.col_name'], ['ev.col_where'], ['ev.col_origin'], ['ev.col_gate'], ['ev.col_verdict'], ['ev.col_quality'], ['ev.col_judge'], ['ev.col_sec_crit_high'], ['ev.col_evaluated']].map(([k, sr], i) => h('th', { scope: 'col', class: `ev-c${i}` }, i === 0 ? h('span', { class: 'sr-only' }, t(sr)) : t(k))))),
        h('tbody', vis.map((r) => this.tr(r, showWhere))))));
  }
  tr(r, showWhere) {
    const on = this.sel.ids.has(r.subject); const n = rowNumbers(r); const when = n.at ? timeText(timeAgo(n.at)) : null;
    const num = (v) => (v == null ? dash() : String(v));
    return h('tr', { class: on ? 'selected' : '', dataset: { subject: r.subject }, onClick: (e) => { if (e.target.closest('a, button, input, select, label')) return; if (e.shiftKey || e.ctrlKey || e.metaKey) this.select(r.subject, { shift: e.shiftKey, ctrl: e.ctrlKey || e.metaKey }); } },
      h('td', { class: 'sel-td' }, h('input', { type: 'checkbox', class: 'sel-box', 'aria-label': t('lib.select_named', { name: r.name }), checked: on, onClick: (e) => { e.stopPropagation(); this.select(r.subject, { shift: e.shiftKey, ctrl: true }); } })),
      h('td', { class: 'ev-name' }, h('button', { type: 'button', class: 'link-btn', 'aria-haspopup': 'dialog', onClick: (e) => this.openDetail(r.subject, e.currentTarget) }, r.name), r.hub_status ? h('div', hubStatusBadge(r.hub_status)) : null),
      h('td', { class: 'ev-where' }, showWhere || isObserved(r) ? h('span', { class: 'muted small' }, `${r.source} · `) : null, h('code', { class: 'small' }, r.path || '-')),
      h('td', originBadge(r.origin)), h('td', gateBadge(r.gate)), h('td', verdictBadge(r)),
      h('td', { class: 'ev-num' }, n.quality ?? dash()), h('td', { class: 'ev-num' }, n.judge ?? dash()),
      h('td', { class: 'ev-num' }, n.critical == null && n.high == null ? dash() : h('span', { class: n.critical || n.high ? 'hot' : '' }, `${num(n.critical)} / ${num(n.high)}`)),
      h('td', { class: 'muted nowrap small' }, when ?? t('ev.never')));
  }
  select(id, mods) { this.sel = applySelection(this.sel, this.order, id, mods); this.paint(); }
  selectedRows() { return this.rows.filter((r) => this.sel.ids.has(r.subject)); }
  paintActions() {
    const n = this.sel.ids.size; const sel = this.selectedRows();
    const obs = importableRows(sel); const clients = clientsOf(sel);
    const importBtn = obs.length ? btn(t('ev.import_btn_n', { count: obs.length }), { icon: 'import', disabled: false, onClick: () => this.importRows(sel) }) : null;
    const force = h('label', { class: 'check ev-force' }, h('input', { type: 'checkbox', id: 'ev-force', checked: !!this.force, onChange: (e) => { this.force = e.target.checked; } }), h('span', t('ev.force')));
    append(clear(this.actions), [this.tierPicker('ev-tiers', this.tiers), force,
      btn(n ? t('ev.eval_selected_n', { count: n }) : t('ev.eval_selected'), { icon: 'play', kind: 'primary', disabled: !n || !this.status?.installed, onClick: () => this.run(runBody({ subjects: [...this.sel.ids], tiers: this.tiers, force: this.force })) }),
      btn(t('ev.eval_all_new'), { icon: 'restart', disabled: !this.status?.installed, onClick: () => this.evalNew() }), importBtn,
      n ? btn(t('lib.clear_selection'), { icon: 'close', kind: 'ghost', onClick: () => { this.sel = { ids: new Set(), anchor: null }; this.paint(); } }) : null,
      clients.length > 1 ? h('span', { class: 'note warn' }, t('ev.import_one_client')) : null]);
    if (importBtn && clients.length > 1) importBtn.disabled = true;
  }
  async evalNew() {
    const label = this.f.source === 'all' ? t('ev.source_all') : this.f.source === 'hub' ? t('ev.source_hub') : (this.clients.find((c) => c.id === this.f.source)?.display_name || this.f.source);
    if (!(await confirmDialog(t('ev.eval_all_confirm', { source: label, tiers: t(`ev.tier_${tierKey(this.tiers)}`) }), { confirmLabel: t('ev.eval_all_new') }))) return;
    this.run(runBody({ select: 'none_or_stale', source: this.f.source, tiers: this.tiers, force: this.force }));
  }
  async observe(client) {
    try { const r = await this.api.observe(client); toast(t('ev.observed_toast', { count: r.total }), { kind: r.complete ? 'ok' : 'warn' }); notifyChanged(); await this.reload(false); } catch (e) { toastError(e); }
  }

  // ---- drawer ----
  openDetail(subject, opener) {
    const host = h('div'); const dr = openDrawer(t('ev.detail_title', { name: this.rowById.get(subject)?.name || splitSubject(subject).key }), host);
    this.drawerReload = () => paint(); dr.el.addEventListener('close', () => { this.drawerReload = null; opener?.focus?.(); });
    let tierValue = this.tiers;
    const paint = () => this.loadInto(host, async () => {
      const d = await this.api.evalDetail(subject);
      const picker = this.tierPicker('ev-dtiers', tierValue, (v) => { tierValue = v; });
      return detailBody(d, { api: this.api, tiersPicker: picker, gateMode: this.gateMode,
        onEvaluate: async () => { await this.run(runBody({ subjects: [subject], tiers: tierValue, force: true })); paint(); },
        onImport: (row) => { dr.close(); this.importRows([this.rowById.get(row.subject) || row]); } });
    }, { skeletonRows: 6 });
    paint();
  }
  importRows(rows) {
    const clients = clientsOf(rows); const obs = importableRows(rows);
    if (clients.length !== 1) { toast(t('ev.import_one_client'), { kind: 'warn' }); return; }
    const c = this.clients.find((x) => x.id === clients[0]);
    openImportDialog({ api: this.api, client: clients[0], clientName: c?.display_name, rows: obs, gateMode: this.gateObserved, onDone: () => { notifyChanged(); this.reload(); } });
  }
}
customElements.define('ah-evaluations', AhEvaluations);
