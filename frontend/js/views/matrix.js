// Matrix: items (grouped by collection) x clients. Cells are tri-state-plus (on / off / via collection / unsupported / blocked).
// Clicks are STAGED locally (undo-able) and only written to the content working tree when the user saves.
import { AhView } from '../components/base.js';
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { store } from '../app-context.js';
import { local } from '../store.js';
import { toast, toastError } from '../components/toast.js';
import { btn, kindBadge, emptyBox, notifyChanged } from '../components/ui.js';
import { colorDot } from '../components/parts.js';
import { cellKey, displayState, stageToggle, stageMany, undo, stagedCount, bulkTarget, toBulkCalls, summarize, isToggleable, isEnabledState, emptyModel } from '../matrix.js';
import { editHref } from '../data.js';
import { bulkOutcome } from '../adapt.js';
import { debounce } from '../util.js';

const GLYPH = { on: 'check', off: null, via_collection: 'collections', unsupported: 'minus', blocked: 'lock' };

class AhMatrix extends AhView {
  setup() {
    this.collapsed = new Set(JSON.parse(local.get('ah.mx.collapsed', '[]')));
    this.q = ''; this.kind = '';
    this.head = h('div', { class: 'row between wrap gap page-head' }, h('h1', t('mx.title')));
    this.toolbar = h('div', { class: 'row gap wrap mx-toolbar' });
    this.wrap = h('div', { class: 'card mx-card' });
    this.bar = h('div', { class: 'unsaved-bar', role: 'region', 'aria-label': t('mx.unsaved_label'), hidden: true });
    this.live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.append(this.head, this.toolbar, this.wrap, this.bar, this.live);
    this.listen(window, 'keydown', (e) => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'z' && !e.target.closest?.('input, textarea')) { e.preventDefault(); this.undo(); } });
    this.listen(window, 'ah:changed', () => { if (!this.saving) this.load(true); });
    this.load();
  }
  get model() { return { staged: store.get().staged, undo: store.get().stagedUndo }; }
  set model(m) { store.set({ staged: m.staged, stagedUndo: m.undo }); }
  async load(quiet) {
    const go = async () => { this.data = await this.api.matrix(); this.buildToolbar(); this.paint(); };
    if (quiet) { try { await go(); } catch { /* keep the old grid */ } return; }
    await this.loadInto(this.wrap, async () => { this.data = await this.api.matrix(); return null; }, { skeletonRows: 8 });
    if (this.data) { this.buildToolbar(); this.paint(); }
  }
  buildToolbar() {
    clear(this.toolbar).append(
      h('div', { class: 'search-box grow' }, icon('search', 16), h('input', { type: 'search', placeholder: t('mx.filter_ph'), 'aria-label': t('mx.filter'), value: this.q, onInput: debounce((e) => { this.q = e.target.value.toLowerCase(); this.paint(); }, 100) })),
      h('select', { 'aria-label': t('lib.f_kind'), onChange: (e) => { this.kind = e.target.value; this.paint(); } }, [h('option', { value: '' }, t('lib.all_kinds')), ...['skill', 'agent', 'instruction', 'mcp', 'rule', 'memory'].map((k) => h('option', { value: k, selected: this.kind === k }, t(`kind.${k}s`)))]),
      btn(t('mx.collapse_all'), { icon: 'chevron', onClick: () => { this.data.groups.forEach((g) => this.collapsed.add(g.id)); this.saveCollapsed(); this.paint(); } }),
      btn(t('mx.expand_all'), { icon: 'chevdown', onClick: () => { this.collapsed.clear(); this.saveCollapsed(); this.paint(); } }));
  }
  saveCollapsed() { local.set('ah.mx.collapsed', JSON.stringify([...this.collapsed])); }
  rowVisible(r) { return (!this.kind || r.kind === this.kind) && (!this.q || `${r.name} ${r.title}`.toLowerCase().includes(this.q)); }
  targets(rows, clients) { return rows.flatMap((r) => clients.map((c) => ({ client: c, kind: r.kind, name: r.name, cell: r.cells[c] }))); }
  stage(fn, message) {
    this.model = fn(this.model);
    this.live.textContent = message || t('mx.staged_n', { count: stagedCount(this.model) });
    this.paint();
  }
  toggleCell(r, client) {
    const cell = r.cells[client];
    if (!isToggleable(cell)) { toast(cell.reason || t(`mx.state_${cell.state}`), { kind: 'warn' }); return; }
    this.stage((m) => stageToggle(m, { client, kind: r.kind, name: r.name }, cell));
  }
  toggleMany(targets, label) {
    const en = bulkTarget(targets, this.model);
    if (en == null) { toast(t('mx.nothing_toggleable'), { kind: 'warn' }); return; }
    this.stage((m) => stageMany(m, targets, en), t(en ? 'mx.bulk_on' : 'mx.bulk_off', { what: label }));
  }
  undo() { if (!this.model.undo.length) return; this.model = undo(this.model); this.live.textContent = t('mx.undone'); this.paint(); }
  paint() {
    const focusKey = document.activeElement?.dataset?.key;
    const d = this.data; const cols = d.columns; const model = this.model;
    const groups = d.groups.map((g) => ({ ...g, rows: g.rows.filter((r) => this.rowVisible(r)) })).filter((g) => g.rows.length);
    if (!d.groups.some((g) => g.rows.length)) { clear(this.wrap).append(emptyBox(t('mx.empty_title'), t('mx.empty_hint'), h('a', { class: 'btn primary', href: '#/import' }, t('lib.empty_import')))); this.paintBar(); return; }
    const allRows = groups.flatMap((g) => g.rows);
    const thead = h('thead', h('tr', h('th', { scope: 'col', class: 'mx-corner' }, t('mx.item')),
      cols.map((c) => { const on = allRows.filter((r) => isEnabledState(r.cells[c.id]?.state)).length; const th = h('th', { scope: 'col', class: 'mx-col' },
        h('button', { type: 'button', class: 'mx-colbtn', title: t('mx.col_toggle', { client: c.display_name, count: allRows.length }), onClick: () => this.toggleMany(this.targets(allRows, [c.id]), c.display_name) },
          h('span', { class: 'mx-colname' }, c.display_name), h('span', { class: 'mx-count' }, t('mx.enabled_count', { n: on, total: allRows.length }))));
        if (c.color) th.style.borderTopColor = c.color; return th; })));
    const tbody = h('tbody');
    for (const g of groups) {
      const closed = this.collapsed.has(g.id);
      tbody.append(h('tr', { class: 'mx-group' }, h('th', { scope: 'rowgroup', class: 'mx-gh' },
        h('button', { type: 'button', class: 'mx-gtoggle', 'aria-expanded': String(!closed), onClick: () => { closed ? this.collapsed.delete(g.id) : this.collapsed.add(g.id); this.saveCollapsed(); this.paint(); } }, icon(closed ? 'chevron' : 'chevdown', 14), colorDot(g.color), h('span', g.title), h('span', { class: 'tab-count' }, String(g.rows.length)))),
        cols.map((c) => h('td', { class: 'mx-gcell' }, h('button', { type: 'button', class: 'btn ghost sm', title: t('mx.group_toggle', { group: g.title, client: c.display_name }), 'aria-label': t('mx.group_toggle', { group: g.title, client: c.display_name }), onClick: () => this.toggleMany(this.targets(g.rows, [c.id]), `${g.title} / ${c.display_name}`) }, icon('grid', 14))))));
      if (closed) continue;
      for (const r of g.rows) {
        tbody.append(h('tr', { class: 'mx-row' },
          h('th', { scope: 'row', class: 'mx-item' }, h('div', { class: 'mx-item-in' }, kindBadge(r.kind), h('a', { href: editHref(r.kind, r.name), title: r.description }, r.name),
            h('button', { type: 'button', class: 'btn ghost sm mx-rowbtn', title: t('mx.row_toggle', { name: r.name }), 'aria-label': t('mx.row_toggle', { name: r.name }), onClick: () => this.toggleMany(this.targets([r], cols.map((c) => c.id)), r.name) }, icon('grid', 14)))),
          cols.map((c) => this.cell(r, c))));
      }
    }
    clear(this.wrap).append(h('div', { class: 'tablewrap mx-wrap', tabindex: '0', role: 'region', 'aria-label': t('mx.scroll_label') }, h('table', { class: 'matrix', 'aria-label': t('mx.table_label') }, thead, tbody)), this.legend());
    this.paintBar();
    if (focusKey) this.querySelector(`[data-key="${CSS.escape(focusKey)}"]`)?.focus();
  }
  cell(r, c) {
    const raw = r.cells[c.id] || { state: 'unsupported' };
    const key = cellKey(c.id, r.kind, r.name);
    const ds = displayState(raw, this.model.staged, key);
    const stateText = t(`mx.state_${ds.state}`);
    const label = `${r.name} · ${c.display_name}: ${stateText}${ds.pending ? ` (${t('mx.unsaved')})` : ''}${ds.reason ? ` - ${ds.reason}` : ''}`;
    const disabled = !isToggleable(raw);
    const b = h('button', { type: 'button', class: `mx-cell ${ds.state}${ds.pending ? ' pending' : ''}`, dataset: { key }, 'aria-label': label, title: ds.reason ? `${stateText}: ${ds.reason}` : stateText, 'aria-disabled': disabled ? 'true' : null, onClick: () => this.toggleCell(r, c.id), onKeydown: (e) => this.arrow(e) },
      GLYPH[ds.state] ? icon(GLYPH[ds.state], 16) : h('span', { class: 'ring' }), ds.state === 'via_collection' ? h('span', { class: 'via-tag' }, t('mx.via')) : null);
    return h('td', { class: 'mx-td' }, b);
  }
  arrow(e) {
    const dirs = { ArrowRight: [0, 1], ArrowLeft: [0, -1], ArrowDown: [1, 0], ArrowUp: [-1, 0] };
    if (!dirs[e.key]) return;
    const rows = [...this.querySelectorAll('tr.mx-row')];
    const tr = e.currentTarget.closest('tr'); const ri = rows.indexOf(tr); const ci = [...tr.querySelectorAll('.mx-cell')].indexOf(e.currentTarget);
    const [dr, dc] = dirs[e.key]; const next = rows[ri + dr]?.querySelectorAll('.mx-cell')[ci + dc];
    if (next) { e.preventDefault(); next.focus(); }
  }
  legend() {
    return h('ul', { class: 'mx-legend', 'aria-label': t('mx.legend') }, ['on', 'via_collection', 'off', 'blocked', 'unsupported'].map((s) => h('li', h('span', { class: `mx-cell static ${s}` }, GLYPH[s] ? icon(GLYPH[s], 14) : h('span', { class: 'ring' })), h('span', t(`mx.legend_${s}`)))),
      h('li', h('span', { class: 'mx-cell static off pending' }, h('span', { class: 'ring' })), h('span', t('mx.legend_pending'))));
  }
  paintBar() {
    const n = stagedCount(this.model);
    this.bar.hidden = n === 0;
    if (!n) return;
    const s = summarize(this.model.staged);
    clear(this.bar).append(h('div', { class: 'unsaved-text' }, h('strong', t('mx.unsaved_n', { count: n })), h('span', { class: 'muted small' }, t('mx.unsaved_detail', { on: s.on, off: s.off }))),
      h('div', { class: 'row gap wrap' },
        btn(t('common.undo'), { icon: 'undo', disabled: !this.model.undo.length, onClick: () => this.undo() }),
        btn(t('mx.discard'), { icon: 'close', onClick: () => { this.model = emptyModel(); this.paint(); } }),
        btn(t('mx.save'), { icon: 'check', onClick: () => this.save(false) }),
        btn(t('mx.save_review'), { icon: 'changes', kind: 'primary', onClick: () => this.save(true) })));
  }
  async save(review) {
    this.saving = true;
    try {
      let changed = 0; const skipped = [];
      for (const call of toBulkCalls(this.model.staged)) { const r = bulkOutcome(await this.api.matrixBulk(call)); changed += r.changed; skipped.push(...r.skipped); }
      this.model = emptyModel();
      toast(t('mx.saved', { count: changed }) + (skipped.length ? ` ${t('mx.skipped', { count: skipped.length })}` : ''), { kind: skipped.length ? 'warn' : 'ok', timeout: 7000 });
      notifyChanged();
      if (review) { location.hash = '#/changes?tab=pending&next=plan'; return; }
      await this.load(true);
    } catch (e) { toastError(e); } finally { this.saving = false; }
  }
}
customElements.define('ah-matrix', AhMatrix);
