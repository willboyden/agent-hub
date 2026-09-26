// Collections: user-defined groups. Sidebar tree (drop targets, reorderable), collection page (members, enable-for-clients),
// create wizard. Every drag-and-drop action has a keyboard alternative: "Move to collection..." picker and up/down buttons.
import { AhView } from '../components/base.js';
import { h, icon, clear } from '../dom.js';
import { t } from '../i18n.js';
import { toast, toastError } from '../components/toast.js';
import { btn, kindBadge, emptyBox, field, notifyChanged, chip } from '../components/ui.js';
import { colorDot } from '../components/parts.js';
import { pickCollections } from '../components/picker.js';
import { openModal, confirmDialog } from '../components/dialog.js';
import { wizard } from '../components/wizard.js';
import { loadAllItems, editHref } from '../data.js';
import { moveRelative, moveBy, dropPosition, addMembers, memberKey } from '../reorder.js';
import { itemId, searchScore } from '../filter.js';
import { slug, validName, readableText } from '../util.js';
import { collectionDoc } from '../adapt.js';
import { suggestionsPanel } from '../components/suggestions.js';

export const COLLECTION_COLORS = ['#0072b2', '#e69f00', '#009e73', '#cc79a7', '#56b4e9', '#d55e00', '#f0e442', '#a6a6a6'];
export const COLLECTION_ICONS = ['folder', 'star', 'branch', 'server', 'shield', 'film', 'layout', 'terminal', 'box', 'knowledge', 'tag', 'library'];

class AhCollections extends AhView {
  setup() {
    this.cid = this.params?.id || null;
    this.drag = null;
    this.side = h('aside', { class: 'coll-side card', 'aria-label': t('coll.sidebar') });
    this.main = h('section', { class: 'coll-main' });
    this.live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.append(h('div', { class: 'row between wrap gap page-head' }, h('h1', t('coll.title')), btn(t('coll.new'), { icon: 'plus', kind: 'primary', onClick: () => this.wizard() })),
      h('div', { class: 'coll-layout' }, this.side, this.main), this.live);
    this.listen(window, 'ah:changed', () => this.reload(true));
    this.reload();
  }
  async reload(quiet) {
    const go = async () => {
      const [collections, items, clients, profiles] = await Promise.all([this.api.collections(), loadAllItems(), this.api.clients()]);
      this.collections = collections.sort((a, b) => a.order - b.order); this.items = items.filter((i) => !i.locked); this.clients = clients;
      this.itemsById = Object.fromEntries(this.items.map((i) => [itemId(i), i]));
      this.profiles = Object.fromEntries(await Promise.all(clients.map(async (c) => [c.id, await this.api.profile(c.id).catch(() => ({ collections: [] }))])));
    };
    if (quiet) { try { await go(); this.paint(); } catch { /* keep */ } return; }
    await this.loadInto(this.main, async () => { await go(); return null; }, { skeletonRows: 4 });
    if (this.collections) this.paint();
  }
  say(msg) { this.live.textContent = ''; setTimeout(() => { this.live.textContent = msg; }, 30); }
  paint() {
    this.paintSide(); this.cid ? this.paintDetail() : this.paintOverview();
    // Newly created collections (e.g. from an accepted suggestion) flash in the tree and the cards so the user sees where the items went.
    const ids = new Set(this.collections.map((c) => c.id));
    if (this.known) for (const id of ids) if (!this.known.has(id)) this.querySelectorAll(`[data-id="${CSS.escape(id)}"]`).forEach((n) => n.classList.add('flash'));
    this.known = ids;
  }

  // ---- sidebar tree ----
  paintSide() {
    const ul = h('ul', { class: 'coll-tree', role: 'list' });
    this.collections.forEach((c, idx) => {
      const li = h('li', { class: `coll-node${this.cid === c.id ? ' current' : ''}`, draggable: 'true', dataset: { id: c.id }, 'aria-label': t('coll.node_label', { name: c.title, count: c.members.length }) },
        h('span', { class: 'grip', 'aria-hidden': 'true', title: t('coll.drag_reorder') }, icon('grip', 14)),
        h('a', { class: 'coll-link', draggable: 'false', href: `#/collections/${encodeURIComponent(c.id)}`, 'aria-current': this.cid === c.id ? 'page' : null }, colorDot(c.color), icon(c.icon || 'folder', 16), h('span', { class: 'ellipsis' }, c.title), h('span', { class: 'tab-count' }, String(c.members.length))),
        h('span', { class: 'coll-move' },
          btn('', { icon: 'up', kind: 'ghost', size: 'sm', title: t('coll.move_up_named', { name: c.title }), disabled: idx === 0, onClick: () => this.moveColl(c.id, -1) }),
          btn('', { icon: 'down', kind: 'ghost', size: 'sm', title: t('coll.move_down_named', { name: c.title }), disabled: idx === this.collections.length - 1, onClick: () => this.moveColl(c.id, 1) })));
      this.dnd(li, { kind: 'coll', id: c.id });
      ul.append(li);
    });
    clear(this.side).append(h('div', { class: 'coll-side-head' }, h('a', { class: `coll-link all${this.cid ? '' : ' current'}`, href: '#/collections' }, icon('collections', 16), h('span', t('coll.overview')))),
      this.collections.length ? ul : emptyBox(t('coll.none_title'), t('coll.none_hint')),
      h('p', { class: 'hint pad' }, t('coll.dnd_hint')));
  }
  // Drop handling is shared: the same element can be dropped ON (add items / reorder collections) with a before/after marker.
  dnd(el, target) {
    if (target.kind === 'coll') el.addEventListener('dragstart', (e) => { this.drag = { type: 'coll', id: target.id }; e.dataTransfer.setData('text/plain', target.id); e.dataTransfer.effectAllowed = 'move'; el.classList.add('dragging'); });
    if (target.kind === 'member') el.addEventListener('dragstart', (e) => { this.drag = { type: 'member', ref: target.ref }; e.dataTransfer.setData('text/plain', target.ref); e.dataTransfer.effectAllowed = 'move'; el.classList.add('dragging'); });
    el.addEventListener('dragend', () => { this.drag = null; el.classList.remove('dragging'); this.clearMarks(); });
    el.addEventListener('dragover', (e) => {
      const d = this.drag; if (!d) return;
      const ok = (target.kind === 'coll' && (d.type === 'items' || d.type === 'coll')) || (target.kind === 'member' && (d.type === 'member' || d.type === 'items')) || (target.kind === 'card' && d.type === 'items');
      if (!ok) return;
      e.preventDefault(); e.dataTransfer.dropEffect = d.type === 'items' && target.kind !== 'member' ? 'copy' : 'move';
      this.clearMarks();
      const r = el.getBoundingClientRect();
      if ((target.kind === 'coll' && d.type === 'coll') || target.kind === 'member') el.classList.add(dropPosition(r.top, r.height, e.clientY) === 'before' ? 'drop-before' : 'drop-after'); else el.classList.add('drop-over');
    });
    el.addEventListener('dragleave', (e) => { if (!el.contains(e.relatedTarget)) el.classList.remove('drop-over', 'drop-before', 'drop-after'); });
    el.addEventListener('drop', (e) => {
      const d = this.drag; if (!d) return; e.preventDefault();
      const r = el.getBoundingClientRect(); const pos = dropPosition(r.top, r.height, e.clientY);
      this.clearMarks(); this.drag = null;
      this.onDrop(d, target, pos);
    });
  }
  clearMarks() { this.querySelectorAll('.drop-over, .drop-before, .drop-after, .dragging').forEach((n) => n.classList.remove('drop-over', 'drop-before', 'drop-after', 'dragging')); }
  async onDrop(d, target, pos) {
    try {
      if (d.type === 'coll' && target.kind === 'coll') {
        const ids = moveRelative(this.collections.map((c) => c.id), d.id, target.id, pos);
        await this.api.reorderCollections(ids); this.say(t('coll.moved_coll', { name: this.collById(d.id).title })); notifyChanged();
      } else if (d.type === 'items' && (target.kind === 'coll' || target.kind === 'card')) {
        await this.api.addMembers(target.id, d.refs); const c = this.collById(target.id);
        toast(t('coll.added_n', { count: d.refs.length, name: c.title }), { kind: 'ok' }); this.say(t('coll.added_n', { count: d.refs.length, name: c.title })); notifyChanged();
      } else if (target.kind === 'member') {
        const c = this.collById(this.cid); const order = c.members.map(memberKey);
        let members = c.members;
        if (d.type === 'items') members = addMembers(members, d.refs); // append, then move next to the target below
        const keys = members.map(memberKey);
        const moving = d.type === 'member' ? [d.ref] : d.refs.map(memberKey);
        let out = keys; for (const k of moving) out = moveRelative(out, k, target.ref, pos);
        void order;
        const map = Object.fromEntries(members.map((m) => [memberKey(m), m]));
        await this.api.updateCollection(c.id, collectionDoc(c, { members: out.map((k) => map[k]) })); this.say(t('coll.reordered')); notifyChanged();
      }
    } catch (e) { toastError(e); }
  }
  collById(id) { return this.collections.find((c) => c.id === id); }
  async moveColl(id, delta) {
    try { await this.api.reorderCollections(moveBy(this.collections.map((c) => c.id), id, delta)); this.say(t('coll.moved_coll', { name: this.collById(id).title })); notifyChanged(); } catch (e) { toastError(e); }
  }

  // ---- overview ----
  paintOverview() {
    const grouped = new Set(this.collections.flatMap((c) => c.members.map(memberKey)));
    const loose = this.items.filter((i) => !grouped.has(itemId(i)));
    const sp = h('div');
    suggestionsPanel({ onAccepted: () => this.reload(true) }).then((el) => { if (el && this._alive) sp.append(el); }).catch(() => {});
    clear(this.main).append(
      sp, h('p', { class: 'muted' }, t('coll.intro')),
      this.collections.length ? h('div', { class: 'grid tiles coll-cards' }, this.collections.map((c) => {
        const card = h('article', { class: 'card coll-card', dataset: { id: c.id }, 'aria-label': c.title },
          h('div', { class: 'coll-card-head' }, colorDot(c.color, 'swatch lg'), icon(c.icon || 'folder', 20), h('h2', h('a', { href: `#/collections/${encodeURIComponent(c.id)}` }, c.title))),
          h('p', { class: 'muted' }, c.description || t('coll.no_description')),
          h('div', { class: 'row gap wrap small' }, ...['skill', 'agent', 'instruction', 'mcp'].map((k) => { const n = c.members.filter((m) => m.kind === k).length; return n ? chip(`${n} ${t(`kind.${k}${n === 1 ? '' : 's'}`)}`) : null; })),
          h('p', { class: 'hint' }, t('coll.drop_here')));
        this.dnd(card, { kind: 'card', id: c.id }); return card;
      })) : emptyBox(t('coll.none_title'), t('coll.none_hint'), btn(t('coll.new'), { icon: 'plus', kind: 'primary', onClick: () => this.wizard() })),
      h('h2', { class: 'section' }, t('coll.loose', { count: loose.length })),
      loose.length ? this.itemPicker(loose, { drag: true }) : h('p', { class: 'muted' }, t('coll.all_grouped')));
  }
  // A list of library items that can be dragged (or moved with the picker button) into collections.
  itemPicker(list, { drag = false } = {}) {
    const sel = new Set();
    const ul = h('ul', { class: 'item-list', role: 'list' });
    const paint = (q = '') => {
      const shown = q ? list.filter((i) => searchScore(q, i) >= 0) : list;
      ul.replaceChildren(...shown.map((it) => {
        const id = itemId(it);
        const li = h('li', { class: 'item-row', draggable: drag ? 'true' : null },
          h('input', { type: 'checkbox', 'aria-label': t('lib.select_named', { name: it.name }), checked: sel.has(id), onChange: (e) => { e.target.checked ? sel.add(id) : sel.delete(id); } }),
          drag ? h('span', { class: 'grip', 'aria-hidden': 'true' }, icon('grip', 14)) : null, kindBadge(it.kind), h('a', { href: editHref(it.kind, it.name) }, it.name), h('span', { class: 'muted small ellipsis grow' }, it.description),
          btn('', { icon: 'collections', kind: 'ghost', size: 'sm', title: t('coll.move_named', { name: it.name }), onClick: () => this.moveToCollection([it]) }));
        if (drag) li.addEventListener('dragstart', (e) => {
          const refs = (sel.has(id) ? [...sel].map((k) => this.itemsById[k]) : [it]).map((x) => ({ kind: x.kind, name: x.name }));
          this.drag = { type: 'items', refs }; e.dataTransfer.setData('text/plain', refs.map((r) => r.name).join(', ')); e.dataTransfer.effectAllowed = 'copyMove'; li.classList.add('dragging');
        });
        if (drag) li.addEventListener('dragend', () => { this.drag = null; this.clearMarks(); });
        return li;
      }));
    };
    paint();
    const q = h('input', { type: 'search', placeholder: t('coll.filter_items'), 'aria-label': t('coll.filter_items'), onInput: (e) => paint(e.target.value) });
    const addSel = btn(t('coll.move_selected'), { icon: 'collections', onClick: () => { const l = [...sel].map((k) => this.itemsById[k]); if (l.length) this.moveToCollection(l); else toast(t('coll.select_first'), { kind: 'warn' }); } });
    return h('div', { class: 'item-picker' }, h('div', { class: 'row gap wrap' }, h('div', { class: 'grow' }, q), addSel), ul);
  }
  async moveToCollection(list) {
    const cur = new Set(this.collections.filter((c) => list.every((it) => c.members.some((m) => m.kind === it.kind && m.name === it.name))).map((c) => c.id));
    const chosen = await pickCollections({ title: list.length === 1 ? t('coll.move_named', { name: list[0].name }) : t('coll.move_n', { count: list.length }), collections: this.collections, current: cur, confirmLabel: t('coll.save_membership') });
    if (!chosen) return;
    try {
      const refs = list.map((i) => ({ kind: i.kind, name: i.name }));
      for (const c of this.collections) {
        if (chosen.includes(c.id) && !cur.has(c.id)) await this.api.addMembers(c.id, refs);
        if (!chosen.includes(c.id) && cur.has(c.id)) for (const r of refs) await this.api.removeMember(c.id, r.kind, r.name);
      }
      toast(t('coll.membership_saved'), { kind: 'ok' }); this.say(t('coll.membership_saved')); notifyChanged();
    } catch (e) { toastError(e); }
  }

  // ---- detail ----
  paintDetail() {
    const c = this.collById(this.cid);
    if (!c) { clear(this.main).append(emptyBox(t('coll.missing'), t('coll.missing_hint'), h('a', { class: 'btn', href: '#/collections' }, t('coll.overview')))); return; }
    const members = c.members.map((m) => ({ m, it: this.itemsById[memberKey(m)] })).filter((x) => x.it);
    const ul = h('ul', { class: 'coll-members', role: 'list', 'aria-label': t('coll.members_label', { name: c.title }) });
    members.forEach(({ m, it }, idx) => {
      const li = h('li', { class: 'member-row', draggable: 'true' },
        h('span', { class: 'grip', 'aria-hidden': 'true', title: t('coll.drag_reorder') }, icon('grip', 14)), kindBadge(it.kind), h('a', { href: editHref(it.kind, it.name) }, it.name),
        h('span', { class: 'muted small ellipsis grow' }, it.description),
        btn('', { icon: 'up', kind: 'ghost', size: 'sm', title: t('coll.member_up', { name: it.name }), disabled: idx === 0, onClick: () => this.moveMember(c, memberKey(m), -1) }),
        btn('', { icon: 'down', kind: 'ghost', size: 'sm', title: t('coll.member_down', { name: it.name }), disabled: idx === members.length - 1, onClick: () => this.moveMember(c, memberKey(m), 1) }),
        btn('', { icon: 'collections', kind: 'ghost', size: 'sm', title: t('coll.move_named', { name: it.name }), onClick: () => this.moveToCollection([it]) }),
        btn('', { icon: 'close', kind: 'ghost', size: 'sm', title: t('coll.remove_named', { name: it.name }), onClick: () => this.removeMember(c, m) }));
      this.dnd(li, { kind: 'member', ref: memberKey(m), id: c.id });
      ul.append(li);
    });
    const inColl = new Set(c.members.map(memberKey));
    const addable = this.items.filter((i) => !inColl.has(itemId(i)));
    clear(this.main).append(
      h('div', { class: 'card coll-head-card' }, h('div', { class: 'card-body' },
        h('div', { class: 'row between wrap gap' }, h('div', { class: 'coll-title' }, colorDot(c.color, 'swatch lg'), icon(c.icon || 'folder', 22), h('h2', c.title)),
          h('div', { class: 'row gap' }, btn(t('common.edit'), { icon: 'editor', onClick: () => this.edit(c) }), btn(t('common.delete'), { icon: 'trash', kind: 'danger', onClick: () => this.remove(c) }))),
        h('p', { class: 'muted' }, c.description || t('coll.no_description')))),
      this.enableCard(c),
      h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('coll.members', { count: members.length }))),
        h('div', { class: 'card-body' }, members.length ? [h('p', { class: 'hint' }, t('coll.reorder_hint')), ul] : emptyBox(t('coll.empty_title'), t('coll.empty_hint')))),
      h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('coll.add_items'))),
        h('div', { class: 'card-body' }, this.addPanel(c, addable))));
    // Dropping library items on the member list appends them (drop on the list itself, not a row).
    this.dnd(ul, { kind: 'card', id: c.id });
  }
  addPanel(c, addable) {
    if (!addable.length) return h('p', { class: 'muted' }, t('coll.everything_in'));
    const sel = new Set();
    const list = h('ul', { class: 'item-list', role: 'list' });
    const paint = (q = '') => list.replaceChildren(...addable.filter((i) => !q || searchScore(q, i) >= 0).slice(0, 60).map((it) => h('li', { class: 'item-row', draggable: 'true' },
      h('input', { type: 'checkbox', 'aria-label': t('lib.select_named', { name: it.name }), checked: sel.has(itemId(it)), onChange: (e) => { e.target.checked ? sel.add(itemId(it)) : sel.delete(itemId(it)); } }), kindBadge(it.kind), h('span', it.name), h('span', { class: 'muted small ellipsis grow' }, it.description))));
    paint();
    return [h('div', { class: 'row gap wrap' }, h('div', { class: 'grow' }, h('input', { type: 'search', placeholder: t('coll.filter_items'), 'aria-label': t('coll.filter_items'), onInput: (e) => paint(e.target.value) })),
      btn(t('coll.add_selected'), { icon: 'plus', kind: 'primary', onClick: async () => { if (!sel.size) { toast(t('coll.select_first'), { kind: 'warn' }); return; } try { await this.api.addMembers(c.id, [...sel].map((k) => { const i = this.itemsById[k]; return { kind: i.kind, name: i.name }; })); toast(t('coll.added_n', { count: sel.size, name: c.title }), { kind: 'ok' }); notifyChanged(); } catch (e) { toastError(e); } } })), list];
  }
  async moveMember(c, key, delta) {
    const map = Object.fromEntries(c.members.map((m) => [memberKey(m), m]));
    try { await this.api.updateCollection(c.id, collectionDoc(c, { members: moveBy(c.members.map(memberKey), key, delta).map((k) => map[k]) })); this.say(t('coll.reordered')); notifyChanged(); } catch (e) { toastError(e); }
  }
  async removeMember(c, m) { try { await this.api.removeMember(c.id, m.kind, m.name); toast(t('coll.removed', { name: m.name }), { kind: 'ok' }); notifyChanged(); } catch (e) { toastError(e); } }
  enableCard(c) {
    const boxes = this.clients.map((cl) => {
      const on = (this.profiles[cl.id]?.collections || []).includes(c.id);
      const supported = c.members.filter((m) => cl.caps?.[{ skill: 'skills', agent: 'agents', instruction: 'instructions', mcp: 'mcp', memory: 'memory', rule: 'permissions' }[m.kind]]).length;
      return h('label', { class: 'check enable-row' }, h('input', { type: 'checkbox', checked: on, onChange: (e) => this.setEnabled(c, cl, e.target.checked) }),
        h('span', { class: 'cdot', style: null }, cl.display_name.charAt(0).toUpperCase()), h('span', cl.display_name), h('span', { class: 'muted small' }, t('coll.client_supports', { n: supported, total: c.members.length })));
    });
    boxes.forEach((b, i) => { const d = b.querySelector('.cdot'); if (this.clients[i].color) { d.style.background = this.clients[i].color; d.style.color = readableText(this.clients[i].color); } });
    return h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t('coll.enable_title'))),
      h('div', { class: 'card-body' }, h('p', { class: 'hint' }, t('coll.enable_hint')), h('div', { class: 'enable-list' }, boxes)));
  }
  async setEnabled(c, cl, on) {
    const prev = this.profiles[cl.id] || { collections: [], enable: {}, disable: {} };
    const next = { ...prev, collections: on ? [...new Set([...prev.collections, c.id])] : prev.collections.filter((x) => x !== c.id) };
    try {
      await this.api.saveProfile(cl.id, { collections: next.collections, enable: prev.enable || {}, disable: prev.disable || {} });
      toast(t(on ? 'coll.enabled_for' : 'coll.disabled_for', { name: c.title, client: cl.display_name }), { kind: 'ok', timeout: 7000, action: { label: t('common.undo'), run: async () => { try { await this.api.saveProfile(cl.id, { collections: prev.collections, enable: prev.enable || {}, disable: prev.disable || {} }); notifyChanged(); } catch (e) { toastError(e); } } } });
      notifyChanged();
    } catch (e) { toastError(e); this.paint(); }
  }
  edit(c) {
    const title = h('input', { type: 'text', value: c.title }), desc = h('textarea', { rows: 3 }, c.description || '');
    const look = this.lookFields(c.icon, c.color);
    const form = h('form', { onSubmit: async (e) => { e.preventDefault(); try { await this.api.updateCollection(c.id, collectionDoc(c, { title: title.value.trim() || c.title, description: desc.value, icon: look.icon(), color: look.color() })); m.close(); notifyChanged(); } catch (err) { toastError(err); } } },
      field(t('coll.f_title'), title), field(t('coll.f_desc'), desc), look.node, h('div', { class: 'row gap end' }, btn(t('common.cancel'), { onClick: () => m.close() }), h('button', { type: 'submit', class: 'btn primary' }, t('common.save'))));
    const m = openModal(t('coll.edit_title'), form); title.focus();
  }
  async remove(c) {
    if (!(await confirmDialog(t('coll.delete_confirm', { name: c.title }), { danger: true, confirmLabel: t('common.delete') }))) return;
    try { await this.api.deleteCollection(c.id); toast(t('coll.deleted', { name: c.title }), { kind: 'ok' }); location.hash = '#/collections'; this.cid = null; notifyChanged(); } catch (e) { toastError(e); }
  }

  // ---- create wizard ----
  lookFields(icn = 'folder', color = COLLECTION_COLORS[0]) {
    let ic = icn, col = color;
    const icons = h('div', { class: 'look-grid', role: 'radiogroup', 'aria-label': t('coll.f_icon') }, COLLECTION_ICONS.map((n) => h('label', { class: 'look-opt', title: n }, h('input', { type: 'radio', name: 'ic', value: n, checked: n === ic, onChange: () => { ic = n; } }), icon(n, 20), h('span', { class: 'sr-only' }, n))));
    const cols = h('div', { class: 'look-grid', role: 'radiogroup', 'aria-label': t('coll.f_color') }, COLLECTION_COLORS.map((c) => { const l = h('label', { class: 'look-opt', title: c }, h('input', { type: 'radio', name: 'col', value: c, checked: c === col, onChange: () => { col = c; } }), colorDot(c, 'swatch lg'), h('span', { class: 'sr-only' }, c)); return l; }));
    return { node: h('div', { class: 'stack-v' }, h('div', h('span', { class: 'lbl' }, t('coll.f_icon')), icons), h('div', h('span', { class: 'lbl' }, t('coll.f_color')), cols)), icon: () => ic, color: () => col };
  }
  wizard() {
    const st = { title: '', id: '', desc: '', members: new Set(), clients: new Set() };
    let look; let idTouched = false;
    const steps = [
      { id: 'name', title: t('coll.w_name'), render: () => {
        const title = h('input', { type: 'text', id: 'cw-title', value: st.title, autocomplete: 'off', placeholder: t('coll.w_title_ph') });
        const id = h('input', { type: 'text', id: 'cw-id', value: st.id, autocomplete: 'off', spellcheck: 'false' });
        const desc = h('textarea', { id: 'cw-desc', rows: 3, placeholder: t('coll.w_desc_ph') }, st.desc);
        title.addEventListener('input', () => { st.title = title.value; if (!idTouched) { st.id = slug(title.value); id.value = st.id; } });
        id.addEventListener('input', () => { idTouched = true; st.id = id.value; });
        desc.addEventListener('input', () => { st.desc = desc.value; });
        return h('div', { class: 'stack-v' }, field(t('coll.f_title'), title), field(t('coll.f_id'), id, t('coll.f_id_hint')), field(t('coll.f_desc'), desc));
      }, valid: () => (!st.title.trim() ? t('coll.w_need_title') : !validName(st.id) ? t('new.bad_name') : this.collections.some((c) => c.id === st.id) ? t('coll.w_id_taken') : true) },
      { id: 'look', title: t('coll.w_look'), render: () => { look = look || this.lookFields(); return look.node; } },
      { id: 'items', title: t('coll.w_items'), render: () => this.wizardItems(st) },
    ];
    const w = wizard({ steps, finishLabel: t('coll.w_create'), onFinish: async () => {
      const c = await this.api.createCollection({ id: st.id, title: st.title.trim(), description: st.desc, icon: look?.icon() || 'folder', color: look?.color() || COLLECTION_COLORS[0], members: [...st.members].map((k) => { const i = this.itemsById[k]; return { kind: i.kind, name: i.name }; }) });
      for (const cid of st.clients) { const p = this.profiles[cid] || { collections: [], enable: {}, disable: {} }; await this.api.saveProfile(cid, { collections: [...new Set([...p.collections, c.id])], enable: p.enable || {}, disable: p.disable || {} }); }
      m.close(); toast(t('coll.created', { name: c.title }), { kind: 'ok' }); notifyChanged(); location.hash = `#/collections/${encodeURIComponent(c.id)}`;
    } });
    const m = openModal(t('coll.new'), w);
  }
  wizardItems(st) {
    const list = h('ul', { class: 'item-list short', role: 'list' });
    const paint = (q = '') => list.replaceChildren(...this.items.filter((i) => !q || searchScore(q, i) >= 0).slice(0, 80).map((it) => h('li', { class: 'item-row' },
      h('input', { type: 'checkbox', 'aria-label': t('lib.select_named', { name: it.name }), checked: st.members.has(itemId(it)), onChange: (e) => { e.target.checked ? st.members.add(itemId(it)) : st.members.delete(itemId(it)); } }), kindBadge(it.kind), h('span', it.name))));
    paint();
    return h('div', { class: 'stack-v' }, h('p', { class: 'hint' }, t('coll.w_items_hint')), h('input', { type: 'search', placeholder: t('coll.filter_items'), 'aria-label': t('coll.filter_items'), onInput: (e) => paint(e.target.value) }), list,
      h('div', h('span', { class: 'lbl' }, t('coll.w_enable')), h('div', { class: 'enable-list' }, this.clients.map((c) => h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: st.clients.has(c.id), onChange: (e) => { e.target.checked ? st.clients.add(c.id) : st.clients.delete(c.id); } }), h('span', c.display_name))))));
  }
}
customElements.define('ah-collections', AhCollections);
