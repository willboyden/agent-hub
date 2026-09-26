// Import wizard: choose a client -> see what it already has (conflicts with per-item choices) -> preview -> import into the working tree.
// A conflicting item can be LINKED to the existing canonical item (default when equal), kept separate (rename), replaced, or skipped.
import { AhView } from '../components/base.js';
import { h, clear, append } from '../dom.js';
import { t } from '../i18n.js';
import { badge, kindBadge, emptyBox, notifyChanged, skeleton, errorBox, btn, seg } from '../components/ui.js';
import { wizard } from '../components/wizard.js';
import { defaultSelection, defaultChoices, previewImport, importItems, importSummary, conflictCount, setAll, itemKey, CHOICES } from '../import-plan.js';
import { KINDS } from '../api.js';

const fresh = (client = '') => ({ client, items: [], sel: new Set(), choices: {}, result: null, notes: [], enable: true });

class AhImport extends AhView {
  setup() {
    this.st = fresh(this.query?.client || '');
    this.host = h('div'); this.append(h('div', { class: 'page-head' }, h('h1', t('imp.title'))), h('p', { class: 'muted' }, t('imp.intro')), this.host);
    this.loadInto(this.host, async () => { this.clients = await this.api.clients(); return this.build(); }, { skeletonRows: 3 });
  }
  build() {
    if (!this.clients.length) return emptyBox(t('cl.empty_title'), t('cl.empty_hint'), h('a', { class: 'btn primary', href: '#/clients' }, t('cl.add')));
    const st = this.st;
    const steps = [
      { id: 'client', title: t('imp.s_client'), render: () => h('div', { class: 'stack-v', role: 'radiogroup', 'aria-label': t('imp.s_client') }, this.clients.map((c) => h('label', { class: `pick-card${st.client === c.id ? ' on' : ''}` },
        h('input', { type: 'radio', name: 'imp-client', value: c.id, checked: st.client === c.id, onChange: () => { st.client = c.id; st.items = []; w.refresh(); } }),
        h('div', h('strong', c.display_name), h('div', { class: 'muted small' }, c.description || c.adapter), h('div', { class: 'muted small' }, Object.values(c.roots || {}).slice(0, 2).join('  ')))))),
        valid: () => (st.client ? true : t('imp.need_client')) },
      { id: 'items', title: t('imp.s_items'), render: () => this.itemsStep(), onEnter: () => { if (!st.items.length && !this.discovering) this.discover(); }, valid: () => (st.sel.size ? true : t('imp.need_items')) },
      { id: 'preview', title: t('imp.s_preview'), render: () => this.previewStep() },
    ];
    const w = wizard({ steps, finishLabel: t('imp.do_import'), onFinish: async () => {
      const prev = previewImport(st.items, st.sel, st.choices);
      const r = await this.api.importItems({ client: st.client, items: importItems(prev), on_conflict: 'skip', enable: st.enable });
      st.result = r; notifyChanged(); clear(this.host).append(this.resultView(r));
    } });
    this.w = w;
    return w;
  }
  async discover() {
    const st = this.st; this.discovering = true; this.discoverError = null;
    try { const r = await this.api.discoverClient(st.client); st.items = r.items || []; st.notes = r.notes || []; st.sel = defaultSelection(st.items); st.choices = defaultChoices(st.items); }
    catch (e) { this.discoverError = e; } finally { this.discovering = false; this.w.refresh(); }
  }
  itemsStep() {
    const st = this.st;
    if (this.discovering || (!st.items.length && !this.discoverError)) return h('div', skeleton(4), h('p', { class: 'muted' }, t('imp.reading')));
    if (this.discoverError) return errorBox(this.discoverError, () => { this.discoverError = null; this.discover(); });
    if (!st.items.length) return emptyBox(t('imp.none_title'), t('imp.none_hint'));
    const groups = KINDS.map((k) => [k, st.items.filter((i) => i.kind === k)]).filter(([, l]) => l.length);
    const bulkHost = h('div');
    const repaint = () => { bulkHost.replaceChildren(...this.bulk(rows)); rows.forEach((r) => r.paint()); };
    const rows = [];
    const tables = groups.map(([k, list]) => h('section', { class: 'card' }, h('header', { class: 'card-head' }, h('h2', t(`kind.${k === 'mcp' ? 'mcps' : k + 's'}`)), h('span', { class: 'muted' }, String(list.length))),
      h('div', { class: 'stack-v tight imp-list' }, list.map((i) => this.itemRow(i, rows, repaint)))));
    repaint();
    return h('div', { class: 'stack-v' }, st.notes.length ? h('p', { class: 'note info' }, st.notes.join(' ')) : null, h('p', { class: 'muted' }, t('imp.found', { count: st.items.length })), bulkHost, ...tables);
  }
  bulk() {
    const st = this.st; const n = conflictCount(st.items, st.sel);
    if (!n) return [h('p', { class: 'note ok' }, t('imp.no_conflicts'))];
    return [h('div', { class: 'note warn' }, h('strong', t('imp.conflicts', { count: n })), ' ', t('imp.conflicts_hint')),
      h('div', { class: 'row gap wrap', role: 'group', 'aria-label': t('imp.bulk') }, h('span', { class: 'muted small' }, t('imp.bulk')),
        ...CHOICES.map((c) => btn(t(`imp.ch_${c}`), { size: 'sm', onClick: () => { Object.assign(st.choices, setAll(st.items, st.sel, c)); this.repaintItems?.(); } })))];
  }
  itemRow(i, rows, repaint) {
    const st = this.st; const key = itemKey(i);
    const box = h('input', { type: 'checkbox', 'aria-label': t('lib.select_named', { name: i.name }), checked: st.sel.has(key), onChange: (e) => { e.target.checked ? st.sel.add(key) : st.sel.delete(key); repaint(); } });
    const ctl = h('div', { class: 'imp-ctl' });
    const diff = h('details', { class: 'imp-diff' });
    const paint = () => {
      ctl.replaceChildren();
      if (!i.conflict) { append(ctl, [badge(t('imp.c_new'), 'ok'), i.floor_violation ? badge(t('imp.floor_violation'), 'bad', i.floor_violation) : null]); diff.hidden = true; return; }
      ctl.append(badge(i.conflict.equal ? t('imp.c_identical') : t('imp.c_different'), i.conflict.equal ? 'muted' : 'warn', i.conflict.existing_source ? t('imp.existing_from', { src: i.conflict.existing_source }) : ''),
        seg(CHOICES.map((c) => ({ value: c, label: t(`imp.ch_${c}`), title: t(`imp.ch_${c}_desc`) })), st.choices[key], (v) => { st.choices[key] = v; }, t('imp.choice_for', { name: i.name })));
      const dl = i.conflict.differences || [];
      diff.hidden = !dl.length; diff.replaceChildren(h('summary', t('imp.differences', { count: dl.length })), h('ul', dl.map((d) => h('li', d))));
    };
    rows.push({ paint });
    this.repaintItems = () => { rows.forEach((r) => r.paint()); };
    return h('div', { class: 'imp-row' }, h('div', { class: 'imp-head' }, box, h('strong', i.name), h('span', { class: 'muted small ellipsis' }, i.description || '')), ctl, diff);
  }
  previewStep() {
    const st = this.st; const prev = previewImport(st.items, st.sel, st.choices); const s = importSummary(prev);
    const kind = { import: 'ok', link: 'info', rename: 'warn', replace: 'warn', skip: 'muted' };
    return h('div', { class: 'stack-v' }, h('p', { class: 'note info' }, t('imp.preview_summary', { writes: s.writes, links: s.links, skip: s.skip })), h('p', { class: 'hint' }, t('imp.preview_hint')),
      h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: st.enable, onChange: (e) => { st.enable = e.target.checked; } }), h('span', t('imp.enable_after', { client: st.client }))),
      h('div', { class: 'tablewrap' }, h('table', { class: 'compact' }, h('thead', h('tr', h('th', { scope: 'col' }, t('lib.col_kind')), h('th', { scope: 'col' }, t('lib.col_name')), h('th', { scope: 'col' }, t('imp.will')))),
        h('tbody', prev.map((p) => h('tr', h('td', kindBadge(p.kind)), h('td', p.name), h('td', badge(t(`imp.a_${p.action}`, { to: p.to }), kind[p.action]))))))));
  }
  resultView(r) {
    const line = (key, list, f) => list.map((x) => `${t(key, f(x))}: ${x.kind}/${x.name}${x.detail ? ` (${x.detail})` : ''}`);
    const rows = [...line('imp.a_import', r.imported, (x) => ({ to: x.final_name })), ...line('imp.a_link', r.linked, () => ({})), ...line('imp.a_rename', r.renamed, (x) => ({ to: x.to })), ...line('imp.a_replace', r.replaced, (x) => ({ to: x.final_name })),
      ...line('imp.a_identical', r.identical, () => ({})), ...line('imp.a_skip', r.skipped, () => ({})), ...line('imp.a_error', r.errors, () => ({}))];
    return h('div', { class: 'stack-v' }, h('div', { class: `note ${r.errors.length ? 'warn' : 'ok'}` }, h('strong', t('imp.done_summary', { created: r.imported.length, linked: r.linked.length, renamed: r.renamed.length, skipped: r.skipped.length })), r.errors.length ? ` ${t('imp.errors', { count: r.errors.length })}` : ''),
      h('ul', { class: 'checks' }, rows.map((x) => h('li', h('span', x)))),
      h('h2', { class: 'section' }, t('imp.next')), h('div', { class: 'row gap wrap' }, h('a', { class: 'btn primary', href: '#/collections' }, t('imp.next_organise')), h('a', { class: 'btn', href: '#/changes' }, t('imp.next_review')), btn(t('imp.again'), { onClick: () => { this.st = fresh(); clear(this.host).append(this.build()); } })));
  }
}
customElements.define('ah-import', AhImport);
