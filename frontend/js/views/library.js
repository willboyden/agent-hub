// Library: every skill/agent/instruction/MCP server/rule/memory in one searchable, filterable, multi-selectable place.
import { AhView } from '../components/base.js';
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { local } from '../store.js';
import { toast, toastError } from '../components/toast.js';
import { btn, badge, kindBadge, emptyBox, seg, field, notifyChanged, timeText, chip } from '../components/ui.js';
import { collectionChip, clientDots, issueBadge, sourceBadge, newItemDialog } from '../components/parts.js';
import { pickCollections } from '../components/picker.js';
import { openModal } from '../components/dialog.js';
import { filterItems, sortItems, facets, itemId, EMPTY_FILTERS, activeFilterCount } from '../filter.js';
import { applySelection, pruneSelection, selectAll } from '../select.js';
import { loadAllItems, byId, editHref } from '../data.js';
import { KINDS } from '../api.js';
import { bulkOutcome } from '../adapt.js';
import { debounce, timeAgo } from '../util.js';

class AhLibrary extends AhView {
  setup() {
    this.mode = local.get('ah.lib.mode', 'cards');
    this.f = { ...EMPTY_FILTERS, ...pick(this.query, Object.keys(EMPTY_FILTERS)) };
    this.sort = this.query.sort || 'name';
    this.sel = { ids: new Set(), anchor: null };
    this.items = []; this.clients = []; this.collections = [];
    this.head = h('div', { class: 'row between wrap gap page-head' }, h('h1', t('lib.title')));
    this.toolbar = h('div', { class: 'lib-toolbar' });
    this.list = h('div', { class: 'lib-list-wrap' });
    this.bulk = h('div', { class: 'bulkbar', role: 'region', 'aria-label': t('lib.bulk_label'), hidden: true });
    this.status = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.append(this.head, this.toolbar, this.list, this.bulk, this.status);
    this.listen(window, 'ah:changed', () => { if (this._alive) this.reload(true); });
    this.listen(this, 'keydown', (e) => { if (e.key === 'Escape' && this.sel.ids.size) { this.sel = { ids: new Set(), anchor: null }; this.paint(); } });
    this.reload();
  }
  async reload(quiet = false) {
    if (!quiet) this.loadInto(this.list, async () => { await this.fetch(); return null; }, { skeletonRows: 6 }).then(() => { if (this._alive && !this.failed) { this.buildToolbar(); this.paint(); } });
    else { try { await this.fetch(); this.paint(); } catch { /* keep the old list */ } }
  }
  async fetch() {
    this.failed = true;
    const [items, clients, collections] = await Promise.all([loadAllItems(), this.api.clients(), this.api.collections()]);
    this.items = items; this.clients = clients; this.collections = collections; this.clientsById = byId(clients); this.collById = byId(collections);
    this.failed = false;
  }
  buildToolbar() {
    const fc = facets(this.items);
    const q = h('input', { type: 'search', id: 'lib-q', placeholder: t('lib.search_ph'), value: this.f.q, 'aria-label': t('lib.search'), autocomplete: 'off',
      onInput: debounce((e) => { this.f.q = e.target.value; if (this.f.q && this.sort !== 'relevance') this.sortPick.value = this.sort = 'relevance'; this.paint(); }, 120) });
    const sel = (id, label, opts, key) => field(label, h('select', { id, onChange: (e) => { this.f[key] = e.target.value; this.paint(); } }, [h('option', { value: '' }, t('lib.any')), ...opts.map((o) => h('option', { value: o.value, selected: this.f[key] === o.value }, o.label))]));
    this.sortPick = h('select', { id: 'lib-sort', onChange: (e) => { this.sort = e.target.value; this.paint(); } },
      ['relevance', 'name', 'updated', 'kind', 'issues', 'enabled'].map((s) => h('option', { value: s, selected: s === this.sort }, t(`lib.sort_${s}`))));
    const kindTabs = h('div', { class: 'kind-chips', role: 'group', 'aria-label': t('lib.filter_kind') },
      [['', t('lib.all_kinds')], ...KINDS.map((k) => [k, t(`kind.${k}s`)])].map(([k, label]) => h('button', { type: 'button', class: 'kchip', dataset: { k }, 'aria-pressed': String(this.f.kind === k), onClick: () => { this.f.kind = k; this.paint(); } }, label, h('span', { class: 'tab-count' }, String(k ? this.items.filter((i) => i.kind === k).length : this.items.length)))));
    this.kindTabs = kindTabs;
    clear(this.toolbar).append(
      h('div', { class: 'row gap wrap' }, h('div', { class: 'grow search-box' }, icon('search', 16), q), seg([{ value: 'cards', label: t('lib.mode_cards'), icon: 'grid' }, { value: 'dense', label: t('lib.mode_dense'), icon: 'list' }], this.mode, (m) => { this.mode = m; local.set('ah.lib.mode', m); this.paint(); }, t('lib.mode')),
        btn(t('lib.new'), { icon: 'plus', kind: 'primary', onClick: () => this.create() })),
      kindTabs,
      h('div', { class: 'filters lib-filters' },
        sel('lib-coll', t('lib.f_collection'), this.collections.map((c) => ({ value: c.id, label: c.title })), 'collection'),
        sel('lib-tag', t('lib.f_tag'), fc.tags.map((x) => ({ value: x.value, label: `${x.value} (${x.n})` })), 'tag'),
        sel('lib-client', t('lib.f_client'), this.clients.map((c) => ({ value: c.id, label: c.display_name })), 'client'),
        sel('lib-enabled', t('lib.f_enabled'), [{ value: 'on', label: t('lib.enabled_on') }, { value: 'off', label: t('lib.enabled_off') }], 'enabled'),
        sel('lib-source', t('lib.f_source'), fc.sources.map((x) => ({ value: x.value, label: `${x.value} (${x.n})` })), 'source'),
        sel('lib-issues', t('lib.f_issues'), [{ value: '1', label: t('lib.issues_only') }, { value: '0', label: t('lib.issues_none') }], 'issues'),
        field(t('lib.sort'), this.sortPick)));
    if (this.f.q && this.sort === 'name') this.sort = 'relevance', this.sortPick.value = 'relevance';
  }
  async create() {
    const r = await newItemDialog({ kind: this.f.kind || 'skill' });
    if (r) location.hash = `#/edit/${r.kind}/${encodeURIComponent(r.name)}?create=1${r.title ? `&title=${encodeURIComponent(r.title)}` : ''}`;
  }
  visible() { return sortItems(filterItems(this.items, this.f), this.sort, this.f.q); }
  paint() {
    if (this.kindTabs) this.kindTabs.querySelectorAll('.kchip').forEach((b) => b.setAttribute('aria-pressed', String(this.f.kind === b.dataset.k)));
    const vis = this.visible(); this.vis = vis;
    this.order = vis.map(itemId);
    this.sel = pruneSelection(this.sel, this.items.map(itemId));
    this.status.textContent = t('lib.result_count', { count: vis.length, total: this.items.length });
    if (!this.items.length) { clear(this.list).append(emptyBox(t('lib.empty_title'), t('lib.empty_hint'), h('div', { class: 'row gap' }, h('a', { class: 'btn primary', href: '#/import' }, t('lib.empty_import')), btn(t('lib.new'), { icon: 'plus', onClick: () => this.create() })))); this.paintBulk(); return; }
    if (!vis.length) {
      clear(this.list).append(emptyBox(t('lib.none_title'), t('lib.none_hint'), btn(t('lib.clear_filters'), { onClick: () => { this.f = { ...EMPTY_FILTERS }; this.buildToolbar(); this.paint(); } }))); this.paintBulk(); return;
    }
    const nf = activeFilterCount(this.f);
    clear(this.list).append(h('div', { class: 'row between wrap gap lib-meta' },
      h('span', { class: 'muted small' }, t('lib.result_count', { count: vis.length, total: this.items.length }), nf ? ` · ${t('lib.filters_active', { count: nf })}` : ''),
      h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: this.sel.ids.size === vis.length && vis.length > 0, indeterminate: this.sel.ids.size > 0 && this.sel.ids.size < vis.length, onChange: (e) => { this.sel = e.target.checked ? selectAll(this.order) : { ids: new Set(), anchor: null }; this.paint(); } }), h('span', t('lib.select_all')))),
      this.mode === 'cards' ? this.cards(vis) : this.dense(vis));
    this.paintBulk();
  }
  selBox(it) {
    const id = itemId(it);
    return h('input', { type: 'checkbox', class: 'sel-box', 'aria-label': t('lib.select_named', { name: it.name }), checked: this.sel.ids.has(id),
      onClick: (e) => { e.stopPropagation(); this.select(id, { shift: e.shiftKey, ctrl: true }); } });
  }
  select(id, mods) { this.sel = applySelection(this.sel, this.order, id, mods); this.paint(); }
  cardClick(e, it) {
    if (e.target.closest('a, button, input, select, label')) return;
    if (e.shiftKey || e.ctrlKey || e.metaKey || this.sel.ids.size) this.select(itemId(it), { shift: e.shiftKey, ctrl: e.ctrlKey || e.metaKey });
  }
  cards(vis) {
    return h('div', { class: 'lib-grid' }, vis.map((it) => {
      const id = itemId(it), on = this.sel.ids.has(id);
      return h('article', { class: `lib-card${on ? ' selected' : ''}`, 'aria-label': `${t(`kind.${it.kind}`)} ${it.name}`, onClick: (e) => this.cardClick(e, it) },
        h('div', { class: 'lib-card-top' }, this.selBox(it), kindBadge(it.kind), issueBadge(it.issues), sourceBadge(it.source)),
        h('h3', { class: 'lib-name' }, it.locked ? h('span', it.name) : h('a', { href: editHref(it.kind, it.name) }, it.name)),
        h('p', { class: 'lib-desc' }, it.description || t('lib.no_description')),
        h('div', { class: 'lib-tags' }, (it.tags || []).slice(0, 4).map((x) => chip(x, { kind: 'tag' })), (it.tags || []).length > 4 ? h('span', { class: 'muted small' }, `+${it.tags.length - 4}`) : null),
        h('div', { class: 'lib-card-foot' },
          h('div', { class: 'lib-colls' }, it.groups.slice(0, 2).map((g) => this.collById[g] && collectionChip(this.collById[g])), it.groups.length > 2 ? h('span', { class: 'muted small' }, `+${it.groups.length - 2}`) : null),
          clientDots(it.enabled_for, this.clientsById)));
    }));
  }
  dense(vis) {
    const rows = vis.map((it) => {
      const id = itemId(it), on = this.sel.ids.has(id);
      return h('tr', { class: on ? 'selected' : '', onClick: (e) => this.cardClick(e, it) },
        h('td', { class: 'sel-td' }, this.selBox(it)),
        h('td', it.locked ? h('span', it.name) : h('a', { class: 'name-link', href: editHref(it.kind, it.name), title: it.description }, it.name)),
        h('td', kindBadge(it.kind)),
        h('td', { class: 'wrap-cell' }, h('div', { class: 'lib-colls' }, it.groups.map((g) => this.collById[g] && collectionChip(this.collById[g])))),
        h('td', { class: 'wrap-cell' }, (it.tags || []).join(', ')),
        h('td', clientDots(it.enabled_for, this.clientsById)),
        h('td', issueBadge(it.issues) || h('span', { class: 'muted' }, '-')),
        h('td', { class: 'muted small nowrap' }, it.updated_at ? timeText(timeAgo(it.updated_at)) : ''));
    });
    return h('div', { class: 'tablewrap' }, h('table', { class: 'compact lib-table' },
      h('thead', h('tr', ['', t('lib.col_name'), t('lib.col_kind'), t('lib.col_collections'), t('lib.col_tags'), t('lib.col_clients'), t('lib.col_issues'), t('lib.col_updated')].map((c, i) => h('th', { scope: 'col' }, i === 0 ? h('span', { class: 'sr-only' }, t('lib.select')) : c)))),
      h('tbody', rows)));
  }
  selectedItems() { return this.items.filter((i) => this.sel.ids.has(itemId(i))); }
  paintBulk() {
    const n = this.sel.ids.size;
    this.bulk.hidden = n === 0;
    if (!n) return;
    clear(this.bulk).append(h('strong', { class: 'bulk-count' }, t('lib.selected', { count: n })),
      btn(t('lib.bulk_collection'), { icon: 'collections', onClick: () => this.bulkCollection() }),
      btn(t('lib.bulk_tag'), { icon: 'tag', onClick: () => this.bulkTags() }),
      btn(t('lib.bulk_enable'), { icon: 'check', onClick: () => this.bulkEnable(true) }),
      btn(t('lib.bulk_disable'), { icon: 'minus', onClick: () => this.bulkEnable(false) }),
      btn(t('lib.clear_selection'), { icon: 'close', kind: 'ghost', onClick: () => { this.sel = { ids: new Set(), anchor: null }; this.paint(); } }));
  }
  refs() { return this.selectedItems().filter((i) => !i.locked).map((i) => ({ kind: i.kind, name: i.name })); }
  async bulkCollection() {
    const ids = await pickCollections({ title: t('lib.bulk_collection'), collections: this.collections, multi: false, confirmLabel: t('lib.add') });
    if (!ids?.length) return;
    try { await this.api.bulkItems({ items: this.refs(), add_to_collection: ids[0] }); toast(t('lib.added_to', { name: this.collById[ids[0]]?.title || ids[0] }), { kind: 'ok' }); notifyChanged(); } catch (e) { toastError(e); }
  }
  bulkTags() {
    const add = h('input', { type: 'text', id: 'bt-add', placeholder: 'review, docs', autocomplete: 'off' }), rem = h('input', { type: 'text', id: 'bt-rem', placeholder: 'legacy', autocomplete: 'off' });
    const split = (s) => s.split(',').map((x) => x.trim()).filter(Boolean);
    const form = h('form', { onSubmit: async (e) => { e.preventDefault(); const a = split(add.value), r = split(rem.value); if (!a.length && !r.length) return; try { await this.api.bulkItems({ items: this.refs(), add_tags: a, remove_tags: r }); m.close(); toast(t('lib.tags_updated'), { kind: 'ok' }); notifyChanged(); } catch (err) { toastError(err); } } },
      field(t('lib.tags_add'), add, t('lib.tags_hint')), field(t('lib.tags_remove'), rem), h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), h('button', { type: 'submit', class: 'btn primary' }, t('common.apply_change'))));
    const m = openModal(t('lib.bulk_tag'), form); add.focus();
  }
  bulkEnable(enabled) {
    const boxes = this.clients.map((c) => h('label', { class: 'pick-row' }, h('input', { type: 'checkbox', value: c.id }), h('span', c.display_name)));
    const form = h('form', { onSubmit: async (e) => { e.preventDefault(); const cl = [...form.querySelectorAll('input:checked')].map((i) => i.value); if (!cl.length) return;
      try { const r = bulkOutcome(await this.api.matrixBulk({ clients: cl, items: this.refs(), enabled })); m.close(); toast(t('lib.enable_done', { changed: r.changed, skipped: r.skipped.length }), { kind: r.skipped.length ? 'warn' : 'ok', timeout: 7000 }); notifyChanged(); } catch (err) { toastError(err); } } },
      h('p', { class: 'hint' }, t(enabled ? 'lib.enable_hint' : 'lib.disable_hint')), h('div', { class: 'pick-list' }, boxes),
      h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), h('button', { type: 'submit', class: 'btn primary' }, t(enabled ? 'lib.bulk_enable' : 'lib.bulk_disable'))));
    const m = openModal(t(enabled ? 'lib.bulk_enable' : 'lib.bulk_disable'), form);
  }
}
const pick = (o, keys) => Object.fromEntries(Object.entries(o || {}).filter(([k]) => keys.includes(k)));
customElements.define('ah-library', AhLibrary);
